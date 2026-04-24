use std::io::IsTerminal;

/// A 4-char bracket status tag for human CLI status rows.
///
/// Used by `lock`, `mount`, and `doctor` to prefix per-item outcome
/// lines. The bracketed form is always 6 columns wide so consecutive
/// rows align.
///
/// Distinct from the dry-run risk tag in `cmd::Step::print_dry_run`,
/// which uses an 11-wide column for `safe` / `destructive` etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTag {
    Ok,
    Warn,
    Fail,
    Skip,
}

pub fn render_status_tag(tag: StatusTag, color_enabled: bool) -> &'static str {
    match (tag, color_enabled) {
        (StatusTag::Ok, false) => "[ok  ]",
        (StatusTag::Warn, false) => "[warn]",
        (StatusTag::Fail, false) => "[fail]",
        (StatusTag::Skip, false) => "[skip]",
        (StatusTag::Ok, true) => "\x1b[32m[ok  ]\x1b[0m",
        (StatusTag::Warn, true) => "\x1b[33m[warn]\x1b[0m",
        (StatusTag::Fail, true) => "\x1b[31m[fail]\x1b[0m",
        (StatusTag::Skip, true) => "\x1b[90m[skip]\x1b[0m",
    }
}

pub fn should_color_status_tags(is_terminal: bool, no_color_active: bool) -> bool {
    is_terminal && !no_color_active
}

/// Pure env-value parser for `NO_COLOR`. A non-empty value disables color;
/// unset or empty does not.
pub fn no_color_active_from_env(value: Option<&std::ffi::OsStr>) -> bool {
    matches!(value, Some(v) if !v.is_empty())
}

pub fn color_enabled_for_stdout() -> bool {
    should_color_status_tags(
        std::io::stdout().is_terminal(),
        no_color_active_from_env(std::env::var_os("NO_COLOR").as_deref()),
    )
}

pub fn color_enabled_for_stderr() -> bool {
    should_color_status_tags(
        std::io::stderr().is_terminal(),
        no_color_active_from_env(std::env::var_os("NO_COLOR").as_deref()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn strip_ansi(input: &str) -> String {
        let mut stripped = String::new();
        let mut chars = input.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for code_ch in chars.by_ref() {
                    if code_ch == 'm' {
                        break;
                    }
                }
            } else {
                stripped.push(ch);
            }
        }
        stripped
    }

    #[test]
    fn status_tag_pins_four_known_levels() {
        // Byte-pin cross-command contract: lock/mount/doctor all rely
        // on these exact strings for column alignment.
        assert_eq!(render_status_tag(StatusTag::Ok, false), "[ok  ]");
        assert_eq!(render_status_tag(StatusTag::Warn, false), "[warn]");
        assert_eq!(render_status_tag(StatusTag::Fail, false), "[fail]");
        assert_eq!(render_status_tag(StatusTag::Skip, false), "[skip]");
    }

    /* Intent: pin the exact ANSI-wrapped status tag bytes.
     * Why it exists: color is only safe if the wrapper starts and
     * ends on the fixed-width tag, not the row body.
     * Scenario: each known status level is rendered for an interactive
     * stream with color enabled.
     */
    #[test]
    fn status_tag_pins_colored_levels() {
        assert_eq!(
            render_status_tag(StatusTag::Ok, true),
            "\x1b[32m[ok  ]\x1b[0m"
        );
        assert_eq!(
            render_status_tag(StatusTag::Warn, true),
            "\x1b[33m[warn]\x1b[0m"
        );
        assert_eq!(
            render_status_tag(StatusTag::Fail, true),
            "\x1b[31m[fail]\x1b[0m"
        );
        assert_eq!(
            render_status_tag(StatusTag::Skip, true),
            "\x1b[90m[skip]\x1b[0m"
        );
    }

    /* Intent: colored tags strip back to the existing plain tags.
     * Why it exists: alignment in redirected output and ANSI-stripped
     * logs depends on the visible tag bytes staying unchanged.
     * Scenario: every colored status tag has its SGR sequences removed
     * and is compared to the plain renderer.
     */
    #[test]
    fn colored_status_tags_strip_to_plain_tags() {
        for tag in [
            StatusTag::Ok,
            StatusTag::Warn,
            StatusTag::Fail,
            StatusTag::Skip,
        ] {
            assert_eq!(
                strip_ansi(render_status_tag(tag, true)),
                render_status_tag(tag, false)
            );
        }
    }

    /* Intent: gate color on both TTY detection and NO_COLOR state.
     * Why it exists: contract output must stay plain when redirected,
     * and NO_COLOR must override an interactive destination.
     * Scenario: the four possible boolean combinations are evaluated
     * through the pure policy helper.
     */
    #[test]
    fn should_color_status_tags_respects_tty_and_no_color() {
        assert!(should_color_status_tags(true, false));
        assert!(!should_color_status_tags(false, false));
        assert!(!should_color_status_tags(true, true));
        assert!(!should_color_status_tags(false, true));
    }

    /* Intent: parse NO_COLOR according to the non-empty value rule.
     * Why it exists: NO_COLOR=0 and NO_COLOR=false still disable color,
     * while an empty value does not.
     * Scenario: representative unset, empty, and non-empty values are
     * passed through the pure parser without mutating process env.
     */
    #[test]
    fn no_color_active_from_env_uses_non_empty_rule() {
        assert!(!no_color_active_from_env(None));
        assert!(!no_color_active_from_env(Some(OsStr::new(""))));
        assert!(no_color_active_from_env(Some(OsStr::new("1"))));
        assert!(no_color_active_from_env(Some(OsStr::new("0"))));
        assert!(no_color_active_from_env(Some(OsStr::new("false"))));
    }
}
