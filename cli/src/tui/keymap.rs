use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::tui::app::Message;

pub fn handle_key(key: KeyEvent) -> Option<Message> {
    match key.code {
        KeyCode::Char('q') => Some(Message::Quit),
        KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => Some(Message::NextTab),
        KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => Some(Message::PrevTab),
        KeyCode::Char('r') => Some(Message::RefreshPool),
        KeyCode::Char('j') | KeyCode::Down => Some(Message::SelectNextDisk),
        KeyCode::Char('k') | KeyCode::Up => Some(Message::SelectPrevDisk),
        _ => None,
    }
}
