use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::Message;
use super::model::ViewMode;

pub fn handle_key(key: KeyEvent, mode: &ViewMode) -> Option<Message> {
    if matches!(
        (key.code, key.modifiers),
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
    ) {
        return Some(Message::Quit);
    }

    if matches!(mode, ViewMode::Help) {
        return Some(Message::ToggleHelp);
    }

    if matches!(mode, ViewMode::SubvolDetail) {
        return match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => Some(Message::PageDown),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => Some(Message::PageUp),
            _ => match key.code {
                KeyCode::Esc | KeyCode::Backspace => Some(Message::Back),
                KeyCode::Char('q') => Some(Message::Quit),
                KeyCode::Char('r') => Some(Message::Reload),
                KeyCode::Char('j') | KeyCode::Down => Some(Message::ScrollDown),
                KeyCode::Char('k') | KeyCode::Up => Some(Message::ScrollUp),
                KeyCode::Char('?') => Some(Message::ToggleHelp),
                _ => None,
            },
        };
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => Some(Message::PageDown),
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => Some(Message::PageUp),
        _ => match key.code {
            KeyCode::Char('q') => Some(Message::Quit),
            KeyCode::Char('?') => Some(Message::ToggleHelp),
            KeyCode::Tab => Some(Message::NextTab),
            KeyCode::BackTab => Some(Message::PrevTab),
            KeyCode::Char('l') | KeyCode::Right => Some(Message::NextSubTab),
            KeyCode::Char('h') | KeyCode::Left => Some(Message::PrevSubTab),
            KeyCode::Char('j') | KeyCode::Down => Some(Message::ScrollDown),
            KeyCode::Char('k') | KeyCode::Up => Some(Message::ScrollUp),
            KeyCode::Enter => Some(Message::Select),
            KeyCode::Char('r') => Some(Message::Reload),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /*
     * Intent: 'r' in SubvolDetail emits Reload.
     *
     * Why it exists: the detail view allow-list must include 'r' so users
     * can refresh the detail without leaving the view.
     *
     * Scenario: user is viewing subvolume detail and presses 'r'.
     */
    #[test]
    fn r_reloads_in_detail_mode() {
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(matches!(
            handle_key(key, &ViewMode::SubvolDetail),
            Some(Message::Reload)
        ));
    }

    /*
     * Intent: Esc in SubvolDetail emits Back.
     *
     * Why it exists: Esc is the standard way to leave a detail view.
     *
     * Scenario: user presses Esc to return to the subvolume list.
     */
    #[test]
    fn esc_goes_back_in_detail() {
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            handle_key(key, &ViewMode::SubvolDetail),
            Some(Message::Back)
        ));
    }

    // Intent: Ctrl-D in SubvolDetail emits PageDown.
    //
    // Why it exists: the detail footer advertises page scrolling, and the
    // detail-mode allow-list must route that key to the existing update path.
    //
    // Scenario: user is reading long subvolume detail output and presses
    // Ctrl-D to move down by one page.
    #[test]
    fn ctrl_d_pages_down_in_detail() {
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_key(key, &ViewMode::SubvolDetail),
            Some(Message::PageDown)
        ));
    }

    // Intent: Ctrl-U in SubvolDetail emits PageUp.
    //
    // Why it exists: the detail footer advertises page scrolling, and the
    // detail-mode allow-list must route that key to the existing update path.
    //
    // Scenario: user is reading long subvolume detail output and presses
    // Ctrl-U to move up by one page.
    #[test]
    fn ctrl_u_pages_up_in_detail() {
        let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_key(key, &ViewMode::SubvolDetail),
            Some(Message::PageUp)
        ));
    }

    /*
     * Intent: Tab key in SubvolDetail is ignored (no tab switching).
     *
     * Why it exists: prevents accidental tab switches while viewing detail.
     *
     * Scenario: user accidentally presses Tab while reading detail output.
     */
    #[test]
    fn tab_in_detail_is_ignored() {
        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert!(handle_key(key, &ViewMode::SubvolDetail).is_none());
    }

    /*
     * Intent: h/l emit PrevSubTab/NextSubTab in Normal mode.
     *
     * Why it exists: subtab navigation via vim-style horizontal movement.
     *
     * Scenario: user presses 'l' to move from Usage to Show subtab.
     */
    #[test]
    fn h_l_switch_subtabs() {
        let l = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
        assert!(matches!(
            handle_key(l, &ViewMode::Normal),
            Some(Message::NextSubTab)
        ));

        let h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
        assert!(matches!(
            handle_key(h, &ViewMode::Normal),
            Some(Message::PrevSubTab)
        ));
    }
}
