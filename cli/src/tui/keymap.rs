use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::app::Message;

pub fn handle_key(key: KeyEvent, show_help: bool, show_disk_detail: bool) -> Option<Message> {
    if let (KeyCode::Char('c'), KeyModifiers::CONTROL) = (key.code, key.modifiers) {
        return Some(Message::Quit);
    }
    if show_help {
        return Some(Message::ToggleHelp);
    }
    // Uppercase R: reset session temperature hi/lo watermarks. Placed
    // after the help guard (so the help overlay's close-on-any-key still
    // wins) and before the disk-detail guard (so R works while the disk
    // detail popup is open, matching the advertised global footer hint).
    if let KeyCode::Char('R') = key.code {
        return Some(Message::ResetTemperatureStats);
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

    // Intent: uppercase R in the main view dispatches ResetTemperatureStats.
    // Why: this is the primary binding advertised in the footer; without
    //      a direct test, a future reshuffle of the match order could
    //      silently drop the mapping and leave the footer hint lying.
    // Scenario: user in the main (non-help, non-detail) view presses
    //           Shift+R after watching a large copy run for a while.
    #[test]
    fn uppercase_r_resets_temperature_stats_in_main() {
        let key = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert!(matches!(
            handle_key(key, false, false),
            Some(Message::ResetTemperatureStats)
        ));
    }

    // Intent: uppercase R works even while the disk-detail popup is open.
    // Why: the plan intentionally places the R handler before the
    //      disk-detail guard so the advertised footer hint isn't silent
    //      inside overlays.
    // Scenario: user opens disk detail, still sees the footer hint,
    //           presses Shift+R.
    #[test]
    fn uppercase_r_resets_temperature_stats_in_disk_detail() {
        let key = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert!(matches!(
            handle_key(key, false, true),
            Some(Message::ResetTemperatureStats)
        ));
    }

    // Intent: uppercase R while help is open does NOT reset; instead it
    //         closes the help overlay via the existing any-key handler.
    // Why: the help overlay owns the "any key dismisses me" contract;
    //      silently mutating session stats from inside an unrelated
    //      overlay would surprise users. `R` is not advertised in help.
    // Scenario: user opens help, presses Shift+R expecting to close it.
    #[test]
    fn uppercase_r_closes_help_not_reset() {
        let key = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert!(matches!(
            handle_key(key, true, false),
            Some(Message::ToggleHelp)
        ));
    }

    // Intent: lowercase r in the main view still dispatches RefreshPool.
    // Why: regression guard for the lowercase/uppercase split. A naive
    //      `KeyCode::Char(c) if c.to_ascii_lowercase() == 'r'` would
    //      collapse both bindings into one.
    // Scenario: user in main view presses r.
    #[test]
    fn lowercase_r_refreshes_pool_in_main() {
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(matches!(
            handle_key(key, false, false),
            Some(Message::RefreshPool)
        ));
    }
}
