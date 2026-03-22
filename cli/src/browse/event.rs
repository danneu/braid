use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ratatui::crossterm::event::{self, KeyEvent};

use crate::cmd::RawCommandOutput;

use super::app::Message;
use super::keymap;
use super::model::ViewMode;

pub enum Event {
    Key(KeyEvent),
    CommandFinished {
        raw: RawCommandOutput,
        generation: u64,
    },
    Tick,
}

impl Event {
    pub fn into_message(self, mode: &ViewMode) -> Option<Message> {
        match self {
            Event::Key(key) => keymap::handle_key(key, mode),
            Event::CommandFinished { raw, generation } => {
                Some(Message::CommandFinished { raw, generation })
            }
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
                            && thread_tx.send(Event::Key(key)).is_err()
                        {
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
