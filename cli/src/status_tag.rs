use std::io::IsTerminal;

/// Bracket status tags for human CLI status rows.
///
/// `render_status_tag` returns the bare bracketed tag. Use
/// `status_line` to produce the canonical 7-column visible prefix for
/// event-log rows. Padding is derived from the status level before
/// color is applied, so ANSI bytes do not affect visible width.
///
/// Distinct from the dry-run risk tag in `cmd::Step::print_dry_run`,
/// which uses an 11-wide column for `safe` / `destructive` etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTag {
    Ok,
    Warn,
    Fail,
    Skip,
    Wait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    Passphrase,
    KeyFile,
}

impl CredentialKind {
    fn label(self) -> &'static str {
        match self {
            CredentialKind::Passphrase => "passphrase",
            CredentialKind::KeyFile => "keyfile",
        }
    }
}

pub fn render_status_tag(tag: StatusTag, color_enabled: bool) -> &'static str {
    match (tag, color_enabled) {
        (StatusTag::Ok, false) => "[ok]",
        (StatusTag::Warn, false) => "[warn]",
        (StatusTag::Fail, false) => "[fail]",
        (StatusTag::Skip, false) => "[skip]",
        (StatusTag::Wait, false) => "[wait]",
        (StatusTag::Ok, true) => "\x1b[32m[ok]\x1b[0m",
        (StatusTag::Warn, true) => "\x1b[33m[warn]\x1b[0m",
        (StatusTag::Fail, true) => "\x1b[31m[fail]\x1b[0m",
        (StatusTag::Skip, true) => "\x1b[90m[skip]\x1b[0m",
        (StatusTag::Wait, true) => "\x1b[90m[wait]\x1b[0m",
    }
}

fn status_tag_pad(tag: StatusTag) -> &'static str {
    match tag {
        StatusTag::Ok => "   ",
        StatusTag::Warn | StatusTag::Fail | StatusTag::Skip | StatusTag::Wait => " ",
    }
}

pub fn status_line(tag: StatusTag, color_enabled: bool, body: &str) -> String {
    format!(
        "{}{}{body}\n",
        render_status_tag(tag, color_enabled),
        status_tag_pad(tag),
    )
}

pub fn emit_status(line: &str) {
    #[cfg(test)]
    {
        if testing::capture_line(line) {
            return;
        }
    }
    eprint!("{line}");
}

pub fn credential_wait_line(kind: CredentialKind, color_enabled: bool, name: &str) -> String {
    status_line(
        StatusTag::Wait,
        color_enabled,
        &format!("{}: checking against {name}...", kind.label()),
    )
}

pub fn credential_ok_line(kind: CredentialKind, color_enabled: bool, name: &str) -> String {
    status_line(
        StatusTag::Ok,
        color_enabled,
        &format!("{}: accepted by {name}", kind.label()),
    )
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
    #[cfg(test)]
    if let Some(enabled) = testing::color_override() {
        return enabled;
    }

    should_color_status_tags(
        std::io::stdout().is_terminal(),
        no_color_active_from_env(std::env::var_os("NO_COLOR").as_deref()),
    )
}

pub fn color_enabled_for_stderr() -> bool {
    #[cfg(test)]
    if let Some(enabled) = testing::color_override() {
        return enabled;
    }

    should_color_status_tags(
        std::io::stderr().is_terminal(),
        no_color_active_from_env(std::env::var_os("NO_COLOR").as_deref()),
    )
}

#[cfg(test)]
pub mod testing {
    use std::cell::RefCell;

    thread_local! {
        static CAPTURED: RefCell<Option<String>> = const { RefCell::new(None) };
        static COLOR_OVERRIDE: RefCell<Option<bool>> = const { RefCell::new(None) };
    }

    pub fn capture_with<F: FnOnce()>(f: F) -> String {
        capture_with_color_override(None, f)
    }

    pub fn capture_with_color<F: FnOnce()>(color_enabled: bool, f: F) -> String {
        capture_with_color_override(Some(color_enabled), f)
    }

    fn capture_with_color_override<F: FnOnce()>(color_enabled: Option<bool>, f: F) -> String {
        struct ResetCapture;

        impl Drop for ResetCapture {
            fn drop(&mut self) {
                CAPTURED.with(|captured| *captured.borrow_mut() = None);
                COLOR_OVERRIDE.with(|color| *color.borrow_mut() = None);
            }
        }

        CAPTURED.with(|captured| *captured.borrow_mut() = Some(String::new()));
        COLOR_OVERRIDE.with(|color| *color.borrow_mut() = color_enabled);
        let _reset = ResetCapture;
        f();
        CAPTURED.with(|captured| captured.borrow_mut().take().unwrap_or_default())
    }

    pub(super) fn color_override() -> Option<bool> {
        COLOR_OVERRIDE.with(|color| *color.borrow())
    }

    pub(super) fn capture_line(line: &str) -> bool {
        CAPTURED.with(|captured| {
            let mut captured = captured.borrow_mut();
            if let Some(buf) = captured.as_mut() {
                buf.push_str(line);
                true
            } else {
                false
            }
        })
    }
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

    /* Intent: the shared status-line writer keeps every visible body
     * start at column 8, regardless of tag length or ANSI color.
     * Why it exists: command output should scan as an event log without
     * callers reimplementing padding around colored or uncolored tags.
     * Scenario: each status level renders a one-byte body in plain and
     * colored modes.
     */
    #[test]
    fn status_line_prefix_is_seven_visible_columns() {
        for tag in [
            StatusTag::Ok,
            StatusTag::Warn,
            StatusTag::Fail,
            StatusTag::Skip,
            StatusTag::Wait,
        ] {
            let plain = status_line(tag, false, "x");
            assert_eq!(plain.find('x'), Some(7), "plain line: {plain:?}");
            assert_eq!(plain[..7].chars().count(), 7);

            let colored = strip_ansi(&status_line(tag, true, "x"));
            assert_eq!(colored.find('x'), Some(7), "colored line: {colored:?}");
            assert_eq!(colored[..7].chars().count(), 7);
        }
    }

    /* Intent: the shared status-line writer leaves the caller-supplied
     * body text intact.
     * Why it exists: row callers own their subject/action wording; the
     * prefix helper must not rewrite or trim the body.
     * Scenario: an Ok line with a short body.
     */
    #[test]
    fn status_line_passes_body_through_unchanged() {
        assert!(status_line(StatusTag::Ok, false, "hello").ends_with("hello\n"));
    }

    #[test]
    fn status_tag_pins_known_levels() {
        // Byte-pin the bare tag strings. `status_line` owns row padding.
        assert_eq!(render_status_tag(StatusTag::Ok, false), "[ok]");
        assert_eq!(render_status_tag(StatusTag::Warn, false), "[warn]");
        assert_eq!(render_status_tag(StatusTag::Fail, false), "[fail]");
        assert_eq!(render_status_tag(StatusTag::Skip, false), "[skip]");
        assert_eq!(render_status_tag(StatusTag::Wait, false), "[wait]");
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
            "\x1b[32m[ok]\x1b[0m"
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
        assert_eq!(
            render_status_tag(StatusTag::Wait, true),
            "\x1b[90m[wait]\x1b[0m"
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
            StatusTag::Wait,
        ] {
            assert_eq!(
                strip_ansi(render_status_tag(tag, true)),
                render_status_tag(tag, false)
            );
        }
    }

    /* Intent: credential verification rows use the shared status-line
     * renderer and fixed wording for both credential kinds.
     * Why it exists: every command that validates a passphrase or keyfile
     * should fill the silent cryptsetup delay with byte-identical rows.
     * Scenario: passphrase and keyfile wait/ok lines render in plain
     * and colored modes.
     */
    #[test]
    fn credential_wait_line_formats_known_credentials() {
        assert_eq!(
            credential_wait_line(CredentialKind::Passphrase, false, "disk1"),
            "[wait] passphrase: checking against disk1...\n"
        );
        assert_eq!(
            credential_wait_line(CredentialKind::KeyFile, false, "disk1"),
            "[wait] keyfile: checking against disk1...\n"
        );
        assert_eq!(
            credential_ok_line(CredentialKind::Passphrase, false, "disk1"),
            "[ok]   passphrase: accepted by disk1\n"
        );
        assert_eq!(
            credential_ok_line(CredentialKind::KeyFile, false, "disk1"),
            "[ok]   keyfile: accepted by disk1\n"
        );
        assert_eq!(
            strip_ansi(&credential_wait_line(
                CredentialKind::Passphrase,
                true,
                "disk1"
            )),
            "[wait] passphrase: checking against disk1...\n"
        );
        assert_eq!(
            strip_ansi(&credential_ok_line(CredentialKind::KeyFile, true, "disk1")),
            "[ok]   keyfile: accepted by disk1\n"
        );
    }

    /* Intent: unit tests can capture status rows without redirecting
     * process-wide stderr.
     * Why it exists: stderr fd redirection races under parallel cargo test;
     * a thread-local seam lets row-emission tests stay deterministic.
     * Scenario: a captured emit_status call returns the rendered line.
     */
    #[test]
    fn testing_capture_with_captures_emit_status() {
        let output = testing::capture_with(|| {
            emit_status(&status_line(StatusTag::Wait, false, "pool: balancing..."));
        });
        assert_eq!(output, "[wait] pool: balancing...\n");
    }

    /* Intent: command tests can force deterministic plain status rows
     * without mutating process-wide color environment.
     * Why it exists: Rust tests run in parallel; changing NO_COLOR or
     * redirecting stderr would create cross-test races.
     * Scenario: a status row emitted through the production stderr color
     * detector is captured with color disabled by the test harness.
     */
    #[test]
    fn testing_capture_with_color_overrides_stderr_color() {
        let output = testing::capture_with_color(false, || {
            emit_status(&status_line(
                StatusTag::Wait,
                color_enabled_for_stderr(),
                "pool: balancing...",
            ));
        });
        assert_eq!(output, "[wait] pool: balancing...\n");
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
