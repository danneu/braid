use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ratatui::crossterm::event::{self, KeyEvent, KeyEventKind};

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
            Event::Key(key) => {
                if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    return None;
                }
                keymap::handle_key(key, mode)
            }
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

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::*;

    fn q_event(kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            kind,
        ))
    }

    // Intent: Release events are dropped before browse keymap dispatch.
    // Why it exists: kitty keyboard protocol can emit a Release after q; that
    // must not produce a second Quit action.
    // Scenario: user presses and releases q in the browse UI.
    #[test]
    fn release_q_is_ignored() {
        assert!(
            q_event(KeyEventKind::Release)
                .into_message(&ViewMode::Normal)
                .is_none()
        );
    }

    // Intent: Press and Repeat events still flow through browse keymap dispatch.
    // Why it exists: the key-kind filter must not drop normal key presses or
    // kitty protocol auto-repeat events.
    // Scenario: user presses q normally, or holds q long enough to generate repeat.
    #[test]
    fn press_and_repeat_q_emit_quit() {
        assert!(matches!(
            q_event(KeyEventKind::Press).into_message(&ViewMode::Normal),
            Some(Message::Quit)
        ));
        assert!(matches!(
            q_event(KeyEventKind::Repeat).into_message(&ViewMode::Normal),
            Some(Message::Quit)
        ));
    }
}
