use std::io::{self, IsTerminal};
use std::time::SystemTime;

use crate::filesystem::Filesystem;
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

/// The naive-local wall clock a UTC instant projects onto, for every surface
/// that compares against a btrfs timestamp.
///
/// btrfs renders scrub ctime as naive local wall-clock (`parse_ctime` yields a
/// `PrimitiveDateTime` with no offset), so a UTC-basis `now` would skew every
/// comparison by the host offset. Both inputs are parameters rather than clock
/// reads so the projection stays unit-testable off the host timezone, and so
/// the actual clock read stays in `main.rs` (the `_at` convention). One
/// function, shared by the TUI's per-frame `now` and the scrub scheduler's
/// freshness `now`, because two projections that disagree would put the
/// operator's "Last scrub" reading and the scheduler's arithmetic on different
/// clocks.
pub fn local_now(utc: time::OffsetDateTime, offset: time::UtcOffset) -> time::PrimitiveDateTime {
    let local = utc.to_offset(offset);
    time::PrimitiveDateTime::new(local.date(), local.time())
}

/// Render a btrfs scrub timestamp in btrfs's own ctime shape.
///
/// Shared by `braid status`'s "Last scrub" row and the scrub scheduler's
/// not-due journal line so an operator comparing the two is reading one
/// timestamp in one format, not two renderings that might disagree.
pub(crate) fn format_scrub_timestamp(ts: &crate::parse::types::ScrubTimestamp) -> String {
    use time::macros::format_description;
    let fmt = format_description!(
        "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]"
    );
    ts.0.format(&fmt).unwrap_or_else(|_| "unknown".to_owned())
}

/// Format an instant as seconds-only UTC RFC3339 (`2023-11-14T22:13:20Z`).
///
/// Shared by the membership corrupt-state sidecar filename and the alert latch
/// `detected_at` stamp, so both surfaces agree on the seconds-only shape that
/// recovery runbooks and the `status` render depend on. Distinct from
/// `now_iso`, which emits the subsecond ISO-8601 form.
pub(crate) fn format_rfc3339_utc_seconds(now: SystemTime) -> String {
    let odt: time::OffsetDateTime = now.into();
    let format = time::format_description::parse("[year]-[month]-[day]T[hour]:[minute]:[second]Z")
        .expect("static format description must parse");
    odt.to_offset(time::UtcOffset::UTC)
        .format(&format)
        .expect("formatting OffsetDateTime as RFC3339 seconds must not fail")
}

/// Parse a stored RFC3339 timestamp back into an `OffsetDateTime` for relative-age
/// computation. `None` on any malformed value so the alert renderer degrades to
/// absolute-only instead of crashing on a hand-edited or future-format latch --
/// the latch stores `detected_at` as an opaque string, so only this render-time
/// parse ever interprets it. Our seconds-only output is valid RFC3339, so the
/// well-known `Rfc3339` description round-trips it.
pub(crate) fn parse_rfc3339_utc(s: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

/// Single relative-age humanizer reused by `braid status` (UTC basis) and the
/// TUI scrub/alert rows (naive-local basis), both of which pass a `time::Duration`
/// so the helper is agnostic to the clock basis. `None` for a negative/future
/// diff (clock skew) so callers drop the relative suffix rather than render a
/// bogus age. The hours bucket exists so a 5-hour age reads `5 hours ago`, not
/// `300 min ago`.
pub(crate) fn humanize_ago(diff: time::Duration) -> Option<String> {
    if diff.is_negative() {
        return None;
    }
    let days = diff.whole_days();
    let hours = diff.whole_hours();
    let minutes = diff.whole_minutes();
    Some(if days > 1 {
        format!("{days} days ago")
    } else if days == 1 {
        "1 day ago".to_owned()
    } else if hours > 1 {
        format!("{hours} hours ago")
    } else if hours == 1 {
        "1 hour ago".to_owned()
    } else if minutes < 1 {
        "<1 min ago".to_owned()
    } else {
        format!("{minutes} min ago")
    })
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
    use super::{
        detail_suffix, format_duration_secs, format_rfc3339_utc_seconds, humanize_ago, local_now,
        parse_rfc3339_utc, require_tty_inner,
    };
    use std::time::{Duration, SystemTime};

    // Intent: local_now projects a UTC instant into naive local wall-clock time.
    // Why it exists: a UTC-basis `now` or an offset sign error would skew both
    //   the TUI's scrub relative-time text and the scrub scheduler's freshness
    //   arithmetic against btrfs's naive-local scrub timestamps.
    // Scenario: the projection runs on hosts in UTC, a negative-offset zone,
    //   and a fractional-offset zone.
    #[test]
    fn local_now_projects_to_host_wall_clock() {
        let utc = time::macros::datetime!(2026-02-24 12:00:00 UTC);

        assert_eq!(
            local_now(utc, time::macros::offset!(-06:00)),
            time::macros::datetime!(2026-02-24 06:00:00)
        );
        assert_eq!(
            local_now(utc, time::macros::offset!(+05:30)),
            time::macros::datetime!(2026-02-24 17:30:00)
        );
        assert_eq!(
            local_now(utc, time::UtcOffset::UTC),
            time::macros::datetime!(2026-02-24 12:00:00)
        );
    }

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

    // Intent: shared timestamp formatting emits seconds-only UTC with a literal
    //   Z suffix (moved here with the helper from membership.rs).
    // Why it exists: the corrupt-state sidecar filename and the alert latch
    //   detected_at stamp both depend on this exact shape; a drift to the
    //   subsecond shape used by now_iso would break recovery runbooks and the
    //   status render.
    // Scenario: a future refactor tries to share timestamp helpers and changes
    //   the seconds-only output.
    #[test]
    fn format_rfc3339_utc_seconds_emits_seconds_only_with_z_suffix() {
        let first = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let second = SystemTime::UNIX_EPOCH;

        assert_eq!(format_rfc3339_utc_seconds(first), "2023-11-14T22:13:20Z");
        assert_eq!(format_rfc3339_utc_seconds(second), "1970-01-01T00:00:00Z");
    }

    // Intent: parse_rfc3339_utc round-trips the seconds-only output of
    //   format_rfc3339_utc_seconds and returns None on a non-RFC3339 string.
    // Why it exists: the status alert renderer parses the stored detected_at
    //   back to compute a relative age, and must degrade to absolute-only
    //   (None) on a hand-edited or malformed latch rather than panic.
    // Scenario: a well-formed latch timestamp parses; an opaque garbage value
    //   does not.
    #[test]
    fn parse_rfc3339_utc_round_trips_and_rejects_garbage() {
        let stamp =
            format_rfc3339_utc_seconds(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert!(
            parse_rfc3339_utc(&stamp).is_some(),
            "valid stamp must parse"
        );
        assert!(
            parse_rfc3339_utc("not a timestamp").is_none(),
            "garbage must parse to None, not panic"
        );
    }

    // Intent: humanize_ago buckets a non-negative Duration into <1 min / N min /
    //   1 hour / N hours / 1 day / N days, and returns None for a future diff.
    // Why it exists: this is the single relative-age humanizer shared by the
    //   status alert line and the TUI scrub/alert rows. The hours bucket is the
    //   regression that distinguishes it from the old TUI-local timeago, which
    //   rendered a 5-hour age as "300 min ago"; the negative-diff None branch is
    //   what keeps a clock-skewed timestamp from printing a bogus age.
    // Scenario: fixed Durations exercise every bucket boundary plus a future diff.
    #[test]
    fn humanize_ago_bucket_boundaries() {
        use time::Duration as TDuration;

        assert_eq!(
            humanize_ago(TDuration::seconds(59)).as_deref(),
            Some("<1 min ago")
        );
        assert_eq!(
            humanize_ago(TDuration::seconds(60)).as_deref(),
            Some("1 min ago")
        );
        assert_eq!(
            humanize_ago(TDuration::minutes(59)).as_deref(),
            Some("59 min ago")
        );
        assert_eq!(
            humanize_ago(TDuration::minutes(60)).as_deref(),
            Some("1 hour ago")
        );
        assert_eq!(
            humanize_ago(TDuration::hours(23)).as_deref(),
            Some("23 hours ago")
        );
        assert_eq!(
            humanize_ago(TDuration::hours(24)).as_deref(),
            Some("1 day ago")
        );
        assert_eq!(
            humanize_ago(TDuration::hours(48)).as_deref(),
            Some("2 days ago")
        );
        assert_eq!(humanize_ago(TDuration::seconds(-1)), None);
    }
}
