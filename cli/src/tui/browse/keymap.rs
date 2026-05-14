use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::app::Message;
use crate::tui::browse::BrowseFocus;
use crate::tui::keymap::KeyContext;

/// Browse-local key dispatcher. Global keys are handled by the parent
/// TUI keymap before this function is reached.
pub(crate) fn handle_key(key: KeyEvent, ctx: &KeyContext) -> Option<Message> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => Some(Message::BrowsePageDown),
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => Some(Message::BrowsePageUp),
        _ => match key.code {
            KeyCode::Char('h') | KeyCode::Left => Some(Message::BrowseFocusLeft),
            KeyCode::Char('l') | KeyCode::Right => Some(Message::BrowseFocusRight),
            KeyCode::Char('j') | KeyCode::Down if ctx.browse_focus == BrowseFocus::Content => {
                Some(Message::BrowseScrollDown)
            }
            KeyCode::Char('j') | KeyCode::Down => Some(Message::BrowseSelectNext),
            KeyCode::Char('k') | KeyCode::Up if ctx.browse_focus == BrowseFocus::Content => {
                Some(Message::BrowseScrollUp)
            }
            KeyCode::Char('k') | KeyCode::Up => Some(Message::BrowseSelectPrev),
            KeyCode::Enter => Some(Message::BrowseEnter),
            KeyCode::Esc | KeyCode::Backspace => Some(Message::BrowseBack),
            KeyCode::Char('r') => Some(Message::BrowseReload),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::browse::state::BrowseFocus;
    use crate::tui::keymap::KeyContext;
    use crate::tui::model::Tab;

    fn ctx(focus: BrowseFocus) -> KeyContext {
        KeyContext {
            tab: Tab::Browse,
            show_help: false,
            show_disk_detail: false,
            browse_focus: focus,
        }
    }

    // Intent: Browse-local h maps to a left focus move.
    // Why it exists: top-level Tab navigation remains global, so h/l are
    // the Browse-specific way to move across columns.
    // Scenario: user moves from Command back to Program.
    #[test]
    fn h_emits_focus_left() {
        let key = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
        assert!(matches!(
            handle_key(key, &ctx(BrowseFocus::Command)),
            Some(Message::BrowseFocusLeft)
        ));
    }

    // Intent: Browse-local l maps to a right focus move.
    // Why it exists: column navigation is owned by Browse, not the
    // top-level tab strip.
    // Scenario: user moves from Program into Command.
    #[test]
    fn l_emits_focus_right() {
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
        assert!(matches!(
            handle_key(key, &ctx(BrowseFocus::Program)),
            Some(Message::BrowseFocusRight)
        ));
    }

    // Intent: content keys emit Browse-specific messages only.
    // Why it exists: update owns whether the focused content can drill in
    // or pop back; keymap stays state-light.
    // Scenario: user presses Enter then Esc inside content.
    #[test]
    fn enter_and_esc_emit_browse_content_messages() {
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            handle_key(enter, &ctx(BrowseFocus::Content)),
            Some(Message::BrowseEnter)
        ));
        assert!(matches!(
            handle_key(esc, &ctx(BrowseFocus::Content)),
            Some(Message::BrowseBack)
        ));
    }

    // Intent: j/k in the content column scroll instead of changing the
    // sidebar selection.
    // Why it exists: content movement must not re-run the active raw
    // command and reset the user's position.
    // Scenario: user pages through a long command output in Browse.
    #[test]
    fn content_j_k_emit_scroll_messages() {
        let down = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(matches!(
            handle_key(down, &ctx(BrowseFocus::Content)),
            Some(Message::BrowseScrollDown)
        ));
        assert!(matches!(
            handle_key(up, &ctx(BrowseFocus::Content)),
            Some(Message::BrowseScrollUp)
        ));
    }

    // Intent: j/k outside content still drive sidebar selection.
    // Why it exists: selecting in Program, Command, and Subview columns
    // immediately loads the corresponding content.
    // Scenario: user moves from Filesystem to Devices in the Command column.
    #[test]
    fn sidebar_j_k_emit_select_messages() {
        let down = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(matches!(
            handle_key(down, &ctx(BrowseFocus::Command)),
            Some(Message::BrowseSelectNext)
        ));
        assert!(matches!(
            handle_key(up, &ctx(BrowseFocus::Command)),
            Some(Message::BrowseSelectPrev)
        ));
    }
}
