mod app;
mod command;
mod effect;
mod event;
mod keymap;
mod state;
mod view;

use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use app::{update, Model};
use effect::execute_effect;
use event::InputHandler;
use view::view;

use crate::config::config_read;

pub fn run(config_path: &Path) -> io::Result<()> {
    let config = config_read(config_path).map_err(|e| io::Error::other(e.to_string()))?;
    let mut terminal = ratatui::init();
    let (_input, cmd_tx, rx) = InputHandler::new();
    let mut model = Model::new(config.disks().to_vec());
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
        terminal.draw(|f| view(model, f))?;

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
