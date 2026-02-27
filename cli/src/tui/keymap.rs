use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::app::Message;

pub fn handle_key(key: KeyEvent, show_help: bool, show_disk_detail: bool) -> Option<Message> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Some(Message::Quit),
        _ => {}
    }
    if show_help {
        return Some(Message::ToggleHelp);
    }
    if show_disk_detail {
        return match key.code {
            KeyCode::Esc | KeyCode::Backspace => Some(Message::CloseDiskDetail),
            KeyCode::Char('q') => Some(Message::Quit),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('q') => Some(Message::Quit),
        KeyCode::Char('?') => Some(Message::ToggleHelp),
        KeyCode::Tab => Some(Message::NextTab),
        KeyCode::BackTab => Some(Message::PrevTab),
        KeyCode::Char('r') => Some(Message::RefreshPool),
        KeyCode::Char('j') | KeyCode::Down => Some(Message::SelectNextDisk),
        KeyCode::Char('k') | KeyCode::Up => Some(Message::SelectPrevDisk),
        KeyCode::Enter => Some(Message::OpenDiskDetail),
        _ => None,
    }
}
