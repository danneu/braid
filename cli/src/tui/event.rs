use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ratatui::crossterm::event::{self, KeyEvent};

use crate::tui::app::Message;
use crate::tui::model::PoolState;
use crate::tui::keymap;
use crate::tui::state::{CmdId, Stream};
use crate::types::MountPoint;

pub enum Event {
    Key(KeyEvent),
    CommandStarted {
        id: CmdId,
        cmd: String,
    },
    CommandOutput {
        id: CmdId,
        stream: Stream,
        line: String,
    },
    CommandFinished {
        id: CmdId,
        status: ExitStatus,
    },
    PoolProbeFinished(Box<Result<Option<PoolState>, String>>, Duration),
    PollRefresh { mount_point: MountPoint },
    Tick,
}

impl Event {
    pub fn into_message(self, show_help: bool, show_disk_detail: bool) -> Option<Message> {
        match self {
            Event::Key(key) => keymap::handle_key(key, show_help, show_disk_detail),
            Event::CommandStarted { id, cmd } => Some(Message::CommandStarted { id, cmd }),
            Event::CommandOutput { id, stream, line } => {
                Some(Message::CommandOutput { id, stream, line })
            }
            Event::CommandFinished { id, status } => Some(Message::CommandFinished { id, status }),
            Event::PoolProbeFinished(result, elapsed) => {
                Some(Message::PoolProbeFinished(result, elapsed))
            }
            Event::PollRefresh { .. } => Some(Message::RefreshPool),
            Event::Tick => Some(Message::Tick),
        }
    }
}

pub struct InputHandler {
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl InputHandler {
    pub fn new() -> (Self, mpsc::Sender<Event>, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_tx = tx.clone();
        let thread_shutdown = shutdown.clone();

        let thread = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => {
                        if let Ok(event::Event::Key(key)) = event::read()
                            && thread_tx.send(Event::Key(key)).is_err() {
                                break;
                            }
                    }
                    Ok(false) => {
                        if thread_tx.send(Event::Tick).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let handler = Self {
            shutdown,
            thread: Some(thread),
        };

        (handler, tx, rx)
    }
}

impl Drop for InputHandler {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
