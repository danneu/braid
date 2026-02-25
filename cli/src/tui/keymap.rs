use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::tui::app::Message;

pub fn handle_key(key: KeyEvent) -> Option<Message> {
    match key.code {
        KeyCode::Char('q') => Some(Message::Quit),
        KeyCode::Char('r') => Some(Message::RefreshPool),
        _ => None,
    }
}
