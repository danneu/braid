use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::tui::app::Message;

pub fn handle_key(key: KeyEvent) -> Option<Message> {
    match key.code {
        KeyCode::Char('q') => Some(Message::Quit),
        KeyCode::Tab => Some(Message::NextTab),
        KeyCode::BackTab => Some(Message::PrevTab),
        KeyCode::Char('r') => Some(Message::RefreshPool),
        KeyCode::Char('j') | KeyCode::Down => Some(Message::SelectNextDisk),
        KeyCode::Char('k') | KeyCode::Up => Some(Message::SelectPrevDisk),
        _ => None,
    }
}
