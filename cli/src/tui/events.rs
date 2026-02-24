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

pub struct EventReceiver {
    rx: mpsc::Receiver<Message>,
}

impl EventReceiver {
    pub fn new(rx: mpsc::Receiver<Message>) -> Self {
        Self { rx }
    }
}

impl EventSource for EventReceiver {
    fn next(&self) -> io::Result<Message> {
        self.rx
            .recv()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}

pub struct CrosstermEventHandler {
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CrosstermEventHandler {
    pub fn new(tick_rate: Duration, tx: mpsc::Sender<Message>) -> Self {
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
                            KeyCode::Char('p' | 'P') => Message::Ping,
                            KeyCode::Char('r' | 'R') => Message::FetchStatus,
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
            shutdown,
            handle: Some(handle),
        }
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
