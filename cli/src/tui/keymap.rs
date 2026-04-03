use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::app::Message;

pub fn handle_key(key: KeyEvent, show_help: bool, show_disk_detail: bool) -> Option<Message> {
    if let (KeyCode::Char('c'), KeyModifiers::CONTROL) = (key.code, key.modifiers) { return Some(Message::Quit) }
    if show_help {
        return Some(Message::ToggleHelp);
    }
    if show_disk_detail {
        return match key.code {
            KeyCode::Esc | KeyCode::Backspace => Some(Message::CloseDiskDetail),
            KeyCode::Char('q') => Some(Message::Quit),
            KeyCode::Char('r') => Some(Message::RefreshPool),
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

#[cfg(test)]
mod tests {
    use super::*;

    // Intent: verify that pressing 'r' while the disk detail popup is open
    //         emits RefreshPool.
    // Why: the keymap guards disk-detail mode with an allow-list of keys;
    //      without this test a future refactor could silently drop 'r' from
    //      that list and break reload-while-viewing.
    // Scenario: user opens disk detail popup, notices stale data, presses 'r'
    //           to refresh without closing the popup.
    #[test]
    fn r_refreshes_pool_in_disk_detail() {
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(matches!(
            handle_key(key, false, true),
            Some(Message::RefreshPool)
        ));
    }
}
