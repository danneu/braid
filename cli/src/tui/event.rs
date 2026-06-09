use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ratatui::crossterm::event::{self, KeyEvent, KeyEventKind};

use crate::tui::app::Message;
use crate::tui::keymap;
use crate::tui::keymap::KeyContext;
use crate::tui::model::{DiskLuksState, FanSnapshot, PoolState, UpsSnapshot};

// Single large variant by design; probe results are rare, and boxing added an extra allocation/deref without a measured benefit -- revisit if profiling shows enum size matters.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    Key(KeyEvent),
    /// Wake-only terminal resize; redraw re-queries the backend size.
    Resize,
    PoolProbeFinished(
        Result<(HashMap<String, DiskLuksState>, Option<PoolState>), String>,
        Duration,
    ),
    FanProbeFinished(FanSnapshot),
    PollPoolRefresh,
    PollFanRefresh,
    UpsProbeFinished(UpsSnapshot),
    PollUpsRefresh,
    BrowseCommandFinished {
        raw: crate::cmd::RawCommandOutput,
        generation: u64,
    },
}

impl Event {
    pub fn into_message(self, ctx: &KeyContext) -> Option<Message> {
        match self {
            Event::Key(key) => {
                if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    return None;
                }
                keymap::handle_key(key, ctx)
            }
            Event::Resize => None,
            Event::PoolProbeFinished(result, elapsed) => {
                Some(Message::PoolProbeFinished(result, elapsed))
            }
            Event::FanProbeFinished(snapshot) => Some(Message::FanProbeFinished(snapshot)),
            Event::PollPoolRefresh => Some(Message::PollPoolRefresh),
            Event::PollFanRefresh => Some(Message::RefreshFan),
            Event::UpsProbeFinished(snapshot) => Some(Message::UpsProbeFinished(snapshot)),
            Event::PollUpsRefresh => Some(Message::RefreshUps),
            Event::BrowseCommandFinished { raw, generation } => {
                Some(Message::BrowseCommandFinished { raw, generation })
            }
        }
    }
}

/// Pure crossterm-to-TUI event mapping so resize forwarding stays covered by
/// unit tests instead of depending on a live terminal input thread.
fn to_tui_event(ev: event::Event) -> Option<Event> {
    match ev {
        event::Event::Key(key) => Some(Event::Key(key)),
        event::Event::Resize(_, _) => Some(Event::Resize),
        _ => None,
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
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if let Some(event) = to_tui_event(ev)
                                && thread_tx.send(event).is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => {}
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
    use ratatui::crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::*;
    use crate::tui::browse::BrowseFocus;
    use crate::tui::model::Tab;

    fn ctx() -> KeyContext {
        KeyContext {
            tab: Tab::Data,
            show_help: false,
            show_disk_detail: false,
            browse_focus: BrowseFocus::Program,
        }
    }

    fn q_event(kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            kind,
        ))
    }

    // Intent: crossterm key and resize events are forwarded into the TUI event
    //         channel, while unsupported terminal events are ignored.
    // Why it exists: the render loop no longer redraws at 60Hz while idle, so
    //         resize must wake it explicitly to avoid stale layout.
    // Scenario: user resizes an idle terminal, presses a key, or the terminal
    //         reports a focus event.
    #[test]
    fn to_tui_event_forwards_resize_and_keys() {
        assert!(matches!(
            to_tui_event(event::Event::Resize(80, 24)),
            Some(Event::Resize)
        ));
        assert!(matches!(
            to_tui_event(event::Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
            ))),
            Some(Event::Key(_))
        ));
        assert!(to_tui_event(event::Event::FocusGained).is_none());
    }

    // Intent: a resize event is a wake-only event and does not dispatch an app
    //         message.
    // Why it exists: ratatui re-queries terminal size during draw, so resize
    //         should trigger redraw without mutating model state.
    // Scenario: user resizes the terminal while no key input or probe result is
    //         pending.
    #[test]
    fn resize_into_message_is_none() {
        assert!(Event::Resize.into_message(&ctx()).is_none());
    }

    // Intent: Release events are dropped before tui keymap dispatch.
    // Why it exists: kitty keyboard protocol can emit a Release after q; that
    // must not produce a second Quit action.
    // Scenario: user presses and releases q in the tui.
    #[test]
    fn release_q_is_ignored() {
        assert!(
            q_event(KeyEventKind::Release)
                .into_message(&ctx())
                .is_none()
        );
    }

    // Intent: Press and Repeat events still flow through tui keymap dispatch.
    // Why it exists: the key-kind filter must not drop normal key presses or
    // kitty protocol auto-repeat events.
    // Scenario: user presses q normally, or holds q long enough to generate repeat.
    #[test]
    fn press_and_repeat_q_emit_quit() {
        assert!(matches!(
            q_event(KeyEventKind::Press).into_message(&ctx()),
            Some(Message::Quit)
        ));
        assert!(matches!(
            q_event(KeyEventKind::Repeat).into_message(&ctx()),
            Some(Message::Quit)
        ));
    }
}
