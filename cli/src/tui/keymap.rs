use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::app::Message;
use crate::tui::browse::BrowseFocus;
use crate::tui::model::Tab;

/// Snapshot of model state needed for routing a key without letting the
/// keymap borrow the full mutable TUI model.
#[derive(Clone, Copy)]
pub(crate) struct KeyContext {
    pub(crate) tab: Tab,
    pub(crate) show_help: bool,
    pub(crate) show_disk_detail: bool,
    pub(crate) browse_focus: BrowseFocus,
}

pub fn handle_key(key: KeyEvent, ctx: &KeyContext) -> Option<Message> {
    if let (KeyCode::Char('c'), KeyModifiers::CONTROL) = (key.code, key.modifiers) {
        return Some(Message::Quit);
    }
    if ctx.show_help {
        return Some(Message::ToggleHelp);
    }

    match key.code {
        KeyCode::Char('q') => Some(Message::Quit),
        KeyCode::Char('?') => Some(Message::ToggleHelp),
        KeyCode::Tab => Some(Message::NextTab),
        KeyCode::BackTab => Some(Message::PrevTab),
        KeyCode::Char('R') => Some(Message::ResetTemperatureStats),
        _ => match ctx.tab {
            Tab::Browse => crate::tui::browse::keymap::handle_key(key, ctx),
            Tab::Data | Tab::Scrub => handle_data_key(key, ctx),
        },
    }
}

fn handle_data_key(key: KeyEvent, ctx: &KeyContext) -> Option<Message> {
    if ctx.show_disk_detail {
        return match key.code {
            KeyCode::Esc | KeyCode::Backspace => Some(Message::CloseDiskDetail),
            KeyCode::Char('r') => Some(Message::RefreshPool),
            _ => None,
        };
    }
    match key.code {
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

    fn ctx(
        tab: Tab,
        show_help: bool,
        show_disk_detail: bool,
        browse_focus: BrowseFocus,
    ) -> KeyContext {
        KeyContext {
            tab,
            show_help,
            show_disk_detail,
            browse_focus,
        }
    }

    fn data_ctx() -> KeyContext {
        ctx(Tab::Data, false, false, BrowseFocus::Program)
    }

    fn browse_ctx(focus: BrowseFocus) -> KeyContext {
        ctx(Tab::Browse, false, false, focus)
    }

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
            handle_key(key, &ctx(Tab::Data, false, true, BrowseFocus::Program),),
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
            handle_key(key, &data_ctx()),
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
            handle_key(key, &ctx(Tab::Data, false, true, BrowseFocus::Program),),
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
            handle_key(key, &ctx(Tab::Data, true, false, BrowseFocus::Program),),
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
            handle_key(key, &data_ctx()),
            Some(Message::RefreshPool)
        ));
    }

    // Intent: global keys stay global even while Browse content has focus.
    // Why it exists: Browse-local column/content navigation must not
    // capture tab switching, help, quit, Ctrl-C, or Shift-R.
    // Scenario: user is inside Browse content and presses a global key.
    #[test]
    fn tab_is_global_across_all_tabs() {
        let ctx = browse_ctx(BrowseFocus::Content);
        assert!(matches!(
            handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &ctx),
            Some(Message::NextTab)
        ));
        assert!(matches!(
            handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &ctx),
            Some(Message::PrevTab)
        ));
        assert!(matches!(
            handle_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &ctx
            ),
            Some(Message::Quit)
        ));
        assert!(matches!(
            handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT), &ctx),
            Some(Message::ToggleHelp)
        ));
        assert!(matches!(
            handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT), &ctx),
            Some(Message::ResetTemperatureStats)
        ));
    }

    // Intent: Browse-only keys route only when the active top tab is Browse.
    // Why it exists: adding h/l/j/k Browse navigation must not change Data
    // and Scrub tab behavior.
    // Scenario: user presses h on Data (ignored) and Browse (moves left).
    #[test]
    fn browse_keys_only_route_on_browse_tab() {
        let h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
        assert!(!matches!(
            handle_key(h, &data_ctx()),
            Some(Message::BrowseFocusLeft)
        ));
        assert!(matches!(
            handle_key(h, &browse_ctx(BrowseFocus::Command)),
            Some(Message::BrowseFocusLeft)
        ));
    }

    // Intent: Data tab disk-selection bindings keep their existing
    // message mapping after the keymap grows Browse routing.
    // Why it exists: the router now branches by tab, so Data's local
    // bindings need direct regression coverage.
    // Scenario: user selects disks and opens detail on Data.
    #[test]
    fn data_tab_keys_unchanged() {
        assert!(matches!(
            handle_key(
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
                &data_ctx()
            ),
            Some(Message::SelectNextDisk)
        ));
        assert!(matches!(
            handle_key(
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
                &data_ctx()
            ),
            Some(Message::SelectPrevDisk)
        ));
        assert!(matches!(
            handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &data_ctx()
            ),
            Some(Message::OpenDiskDetail)
        ));
    }

    // Intent: help overlay swallows every non-Ctrl-C key, including the
    // new Browse-local surface and top-level globals.
    // Why it exists: the TUI has an established "any key closes help"
    // contract that must not regress as Browse adds keybindings.
    // Scenario: user opens help and presses common navigation/reload keys.
    #[test]
    fn help_swallows_q_tab_r_h_l() {
        let ctx = ctx(Tab::Browse, true, false, BrowseFocus::Content);
        let keys = [
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
        ];
        for key in keys {
            assert!(matches!(handle_key(key, &ctx), Some(Message::ToggleHelp)));
        }
    }

    // Intent: Ctrl-C remains the only key that beats the help overlay.
    // Why it exists: users need an unconditional quit path from any TUI
    // state.
    // Scenario: help is open and user presses Ctrl-C.
    #[test]
    fn ctrl_c_still_quits_inside_help() {
        let ctx = ctx(Tab::Browse, true, false, BrowseFocus::Content);
        assert!(matches!(
            handle_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &ctx
            ),
            Some(Message::Quit)
        ));
    }
}
