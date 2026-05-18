use std::io::{self, IsTerminal};

fn require_tty_inner(cmd: &str, stdin_tty: bool, stdout_tty: bool) -> io::Result<()> {
    if stdin_tty && stdout_tty {
        return Ok(());
    }

    Err(io::Error::other(format!("braid {cmd} requires a terminal")))
}

pub fn require_tty(cmd: &str) -> io::Result<()> {
    require_tty_inner(cmd, io::stdin().is_terminal(), io::stdout().is_terminal())
}

pub fn now_iso() -> String {
    use time::format_description::well_known::Iso8601;
    time::OffsetDateTime::now_utc()
        .format(&Iso8601::DEFAULT)
        .expect("formatting UTC as ISO8601 should never fail")
}

/// Renders seconds with unit suffixes so callers never produce the
/// ambiguous `H:MM` vs `M:SS` collision at duration boundaries.
pub(crate) fn format_duration_secs(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}h {}m {}s", h, m, s)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

#[cfg(test)]
mod tests {
    use super::{format_duration_secs, require_tty_inner};

    // Intent: require_tty_inner returns Ok only when both stdin and stdout
    // are terminals.
    // Why it exists: the predicate is the entire contract of the public
    // require_tty wrapper.
    // Scenario: each stdio combination for the surviving TUI caller.
    #[test]
    fn require_tty_inner_blocks_when_either_stdio_is_not_a_tty() {
        assert!(require_tty_inner("tui", true, true).is_ok());

        let e = require_tty_inner("tui", false, true).unwrap_err();
        assert_eq!(e.to_string(), "braid tui requires a terminal");

        let e = require_tty_inner("tui", true, false).unwrap_err();
        assert_eq!(e.to_string(), "braid tui requires a terminal");
    }

    // Intent: format_duration_secs keeps second, minute, and hour branches
    // distinct at boundaries.
    // Why it exists: shared human rendering must not collapse 60s and 3600s
    // into the same clock-looking string.
    // Scenario: UPS runtime and scrub duration rows both call this helper.
    #[test]
    fn format_duration_secs_disambiguates_boundaries() {
        assert_eq!(format_duration_secs(45), "45s");
        assert_eq!(format_duration_secs(60), "1m 0s");
        assert_eq!(format_duration_secs(3599), "59m 59s");
        assert_eq!(format_duration_secs(3600), "1h 0m 0s");
    }
}
