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

#[cfg(test)]
mod tests {
    use super::require_tty_inner;

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
}
