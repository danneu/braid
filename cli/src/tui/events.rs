use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};

use super::app::Message;

pub trait EventSource {
    fn next(&self) -> io::Result<Message>;
}

pub struct CrosstermEventHandler {
    rx: mpsc::Receiver<Message>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CrosstermEventHandler {
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_flag = Arc::clone(&shutdown);

        let handle = thread::spawn(move || {
            while !shutdown_flag.load(Ordering::Relaxed) {
                if event::poll(tick_rate).unwrap_or(false) {
                    if let Ok(Event::Key(key)) = event::read() {
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        let msg = match key.code {
                            KeyCode::Char('q' | 'Q') => Message::Quit,
                            KeyCode::Char('d' | 'D') => Message::ToggleDebug,
                            _ => continue,
                        };
                        if tx.send(msg).is_err() {
                            break;
                        }
                    }
                } else {
                    // Timeout — send tick
                    if tx.send(Message::Tick).is_err() {
                        break;
                    }
                }
            }
        });

        Self {
            rx,
            shutdown,
            handle: Some(handle),
        }
    }
}

impl EventSource for CrosstermEventHandler {
    fn next(&self) -> io::Result<Message> {
        self.rx
            .recv()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}

impl Drop for CrosstermEventHandler {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
