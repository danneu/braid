use std::io::{self, IsTerminal};

use crate::probe::Filesystem;
use crate::types::MountPoint;

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

/// Centralizes the "drop the trailing `: <detail>` clause when detail is
/// blank" rule so command-failure messages never trail a contentless colon
/// at a tool boundary. Callers pass already-trimmed text; the helper keys
/// off `is_empty()` only.
pub(crate) fn detail_suffix(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
}

/// Create the pool mount-point directory through the `Filesystem` seam,
/// surfacing a mkdir failure as a named operator message instead of letting it
/// resurface as a confusing kernel `mount` failure a step later. Idempotent:
/// `create_dir_all` returns Ok when the directory already exists (the NixOS
/// tmpfiles / sealed-dir case) and errors when the directory or a missing
/// parent cannot be created -- for example a path component is a non-directory,
/// or the parent is unwritable or full. (Per std's docs this list is not
/// exhaustive, and some parent directories may have been created before the
/// error.) Returns the message so each caller wraps it in its own error enum.
pub(crate) fn ensure_mount_point_dir<F: Filesystem + ?Sized>(
    fs: &F,
    mount_point: &MountPoint,
) -> Result<(), String> {
    fs.create_dir_all(mount_point.as_str())
        .map_err(|e| format!("could not create mount point {mount_point}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::{detail_suffix, format_duration_secs, require_tty_inner};

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

    // Intent: detail_suffix omits the separator only when the supplied detail
    // is actually empty.
    // Why it exists: command-failure renderers trim stderr at capture sites;
    // the shared suffix helper must not add another normalization boundary.
    // Scenario: a tool exits non-zero with blank stderr, real stderr, or
    // whitespace that a caller deliberately did not trim.
    #[test]
    fn detail_suffix_only_omits_empty_detail() {
        assert_eq!(detail_suffix(""), "");
        assert_eq!(detail_suffix("x"), ": x");
        assert_eq!(detail_suffix("  "), ":   ");
    }
}
