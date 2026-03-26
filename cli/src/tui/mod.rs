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
use crate::membership;
use crate::parse::types::{
    BtrfsBgType, BtrfsDfEntry, BtrfsProfile, DeviceAllocation, ScrubState, ScrubTimestamp,
    SmartHealth,
};
use crate::types::MountPoint;

pub fn run(config_path: &Path) -> io::Result<()> {
    let config = config_read(config_path).map_err(|e| io::Error::other(e.to_string()))?;
    let membership = membership::load_membership()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mut terminal = ratatui::init();
    let (_input, cmd_tx, rx) = InputHandler::new();
    let disk_names: Vec<String> = membership.disks.keys().cloned().collect();
    let disk_by_id: HashMap<String, String> = membership
        .disks
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect();
    let (mut model, init_effects) =
        Model::new(disk_names, disk_by_id, config.mount_point().0.clone());
    for effect in init_effects {
        execute_effect(effect, &cmd_tx);
    }
    let result = run_loop(&mut terminal, &mut model, &rx, &cmd_tx);
    ratatui::restore();
    result
}

pub fn run_demo() -> io::Result<()> {
    let disk_names = vec![
        "toshiba".to_owned(),
        "ironwolf".to_owned(),
        "wdc".to_owned(),
    ];
    let disk_usage = HashMap::from([
        (
            "toshiba".to_owned(),
            DiskUsage {
                size: 6_001_175_126_016,
                allocations: vec![
                    DeviceAllocation {
                        alloc_type: "Data".into(),
                        profile: "RAID1".into(),
                        bytes: 1_483_734_958_080,
                    },
                    DeviceAllocation {
                        alloc_type: "Metadata".into(),
                        profile: "DUP".into(),
                        bytes: 1_610_612_736,
                    },
                    DeviceAllocation {
                        alloc_type: "System".into(),
                        profile: "DUP".into(),
                        bytes: 16_777_216,
                    },
                ],
                unallocated: 4_515_816_777_984,
            },
        ),
        (
            "ironwolf".to_owned(),
            DiskUsage {
                size: 6_001_175_126_016,
                allocations: vec![
                    DeviceAllocation {
                        alloc_type: "Data".into(),
                        profile: "RAID1".into(),
                        bytes: 1_483_734_958_080,
                    },
                    DeviceAllocation {
                        alloc_type: "Metadata".into(),
                        profile: "DUP".into(),
                        bytes: 1_610_612_736,
                    },
                    DeviceAllocation {
                        alloc_type: "System".into(),
                        profile: "DUP".into(),
                        bytes: 16_777_216,
                    },
                ],
                unallocated: 4_515_816_777_984,
            },
        ),
        (
            "wdc".to_owned(),
            DiskUsage {
                size: 4_000_787_030_016,
                allocations: vec![
                    DeviceAllocation {
                        alloc_type: "Data".into(),
                        profile: "RAID1".into(),
                        bytes: 824_633_720_832,
                    },
                    DeviceAllocation {
                        alloc_type: "Metadata".into(),
                        profile: "DUP".into(),
                        bytes: 1_073_741_824,
                    },
                    DeviceAllocation {
                        alloc_type: "System".into(),
                        profile: "DUP".into(),
                        bytes: 16_777_216,
                    },
                ],
                unallocated: 3_175_062_790_144,
            },
        ),
    ]);
    let smart_health = HashMap::from([
        ("toshiba".to_owned(), SmartHealth::Healthy),
        ("ironwolf".to_owned(), SmartHealth::Degraded),
        ("wdc".to_owned(), SmartHealth::Unknown),
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
        mount_point: MountPoint("/mnt/storage".to_owned()),
        df_entries: vec![
            BtrfsDfEntry {
                bg_type: BtrfsBgType::Data,
                bg_profile: BtrfsProfile::Raid1,
                bg_used: 2_308_094_370_816,
                bg_total: 5_937_955_045_376,
            },
            BtrfsDfEntry {
                bg_type: BtrfsBgType::Metadata,
                bg_profile: BtrfsProfile::Raid1,
                bg_used: 1_610_612_736,
                bg_total: 2_147_483_648,
            },
            BtrfsDfEntry {
                bg_type: BtrfsBgType::System,
                bg_profile: BtrfsProfile::Raid1,
                bg_used: 16_384,
                bg_total: 16_777_216,
            },
            BtrfsDfEntry {
                bg_type: BtrfsBgType::GlobalReserve,
                bg_profile: BtrfsProfile::Single,
                bg_used: 0,
                bg_total: 5_767_168,
            },
        ],
        disk_usage,
        disk_transport,
        smart_health,
        luks_info,
        device_errors: HashMap::new(),
        alert_state: crate::alert::AlertState { active: false, causes: vec![] },
        scrub: ScrubState::Completed {
            started_at: ScrubTimestamp(time::macros::datetime!(2026-02-24 02:00:07)),
            error_count: 0,
            duration: Some("0:00:00".to_owned()),
            total: Some("32.36MiB".to_owned()),
            rate: Some("32.34MiB/s".to_owned()),
        },
        balance: crate::status::BalanceReport::Idle,
        capacity_total_bytes: Some(8_001_568_641_024),
        capacity_used_bytes: 2_308_094_370_816,
        probed_at: Instant::now(),
    };
    let mut model = Model::new_demo(disk_names, PoolStatus::Mounted(pool));

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
