use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::tag,
    character::complete::{not_line_ending, space0, u64 as parse_u64},
};

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::helpers::{parse_ctime, parse_duration_hms};
use super::types::{BtrfsScrubStatusOutput, ScrubState, ScrubTimestamp};

// ---------------------------------------------------------------------------
// nom parsers — one per line type
// ---------------------------------------------------------------------------

/// "Scrub started:    Tue Feb 24 02:00:07 2026" → timestamp string
/// Also handles "Scrub resumed:" (emitted when a scrub is resumed).
fn parse_scrub_started_or_resumed(input: &str) -> IResult<&str, &str> {
    let (input, _) = space0(input)?;
    let (input, _) = alt((tag("Scrub started:"), tag("Scrub resumed:"))).parse(input)?;
    let (input, _) = space0(input)?;
    let (input, ts) = not_line_ending(input)?;
    Ok((input, ts.trim()))
}

/// "Status:           running" → "running"
fn parse_status_line(input: &str) -> IResult<&str, &str> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("Status:")(input)?;
    let (input, _) = space0(input)?;
    let (input, value) = not_line_ending(input)?;
    Ok((input, value.trim()))
}

/// "Duration:         0:05:58" → 358 (seconds)
fn parse_duration_line(input: &str) -> IResult<&str, u64> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("Duration:")(input)?;
    let (input, _) = space0(input)?;
    let (input, rest) = not_line_ending(input)?;
    let secs = parse_duration_hms(rest.trim()).ok_or_else(|| {
        nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Fail))
    })?;
    Ok((input, secs))
}

/// "Time left:        0:34:24" → 2064 (seconds)
fn parse_time_left_line(input: &str) -> IResult<&str, u64> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("Time left:")(input)?;
    let (input, _) = space0(input)?;
    let (input, rest) = not_line_ending(input)?;
    let secs = parse_duration_hms(rest.trim()).ok_or_else(|| {
        nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Fail))
    })?;
    Ok((input, secs))
}

/// "ETA:              Thu Apr 16 19:09:10 2026" → ScrubTimestamp
fn parse_eta_line(input: &str) -> IResult<&str, ScrubTimestamp> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("ETA:")(input)?;
    let (input, _) = space0(input)?;
    let (input, rest) = not_line_ending(input)?;
    let ts = parse_ctime(rest.trim()).ok_or_else(|| {
        nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Fail))
    })?;
    Ok((input, ScrubTimestamp(ts)))
}

/// "Total to scrub:   596353253376" → 596353253376
fn parse_total_to_scrub(input: &str) -> IResult<&str, u64> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("Total to scrub:")(input)?;
    let (input, _) = space0(input)?;
    let (input, bytes) = parse_u64(input)?;
    Ok((input, bytes))
}

/// "Bytes scrubbed:   88143626240  (14.78%)" → 88143626240
///
/// Extracts only the leading raw byte count. The parenthesized percentage suffix
/// is consumed opaquely — btrfs computes it via `100.0 * bytes_scrubbed / bytes_total`
/// and can produce nan/inf text under edge cases. The parser must not fail on
/// non-numeric suffix content.
fn parse_bytes_scrubbed(input: &str) -> IResult<&str, u64> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("Bytes scrubbed:")(input)?;
    let (input, _) = space0(input)?;
    let (input, bytes) = parse_u64(input)?;
    // Consume the rest of the line opaquely (e.g. "  (14.78%)")
    let (input, _) = not_line_ending(input)?;
    Ok((input, bytes))
}

/// "Rate:             246211246/s" → 246211246
/// Also handles optional limit suffix: "104857600/s (limit 52428800/s)"
fn parse_rate_line(input: &str) -> IResult<&str, u64> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("Rate:")(input)?;
    let (input, _) = space0(input)?;
    let (input, bytes) = parse_u64(input)?;
    // Consume "/s" and any optional limit suffix
    let (input, _) = not_line_ending(input)?;
    Ok((input, bytes))
}

/// "Error summary:    read=1 csum=2" → 3
/// "Error summary:    no errors found" → 0
/// "Error summary:   " (empty, errors on continuation lines only) → 0
fn parse_error_summary(input: &str) -> IResult<&str, u64> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("Error summary:")(input)?;
    let (input, _) = space0(input)?;
    let (input, rest) = not_line_ending(input)?;
    let rest = rest.trim();
    if rest == "no errors found" || rest.is_empty() {
        return Ok((input, 0));
    }
    // e.g. "read=1 csum=2" — sum all values
    let count: u64 = rest
        .split_whitespace()
        .filter_map(|kv| kv.split('=').nth(1))
        .filter_map(|v| v.parse::<u64>().ok())
        .fold(0u64, |acc, value| acc.saturating_add(value));
    Ok((input, count))
}

/// "  Corrected:      2" → 2
/// "  Uncorrectable:  1" → 1
/// "  Unverified:     0" → 0
///
/// These continuation lines appear after Error summary when any error bucket
/// is nonzero (scrub.c:245-247). Their values must be included in error_count
/// to avoid reporting 0 when corrected/uncorrectable errors exist.
fn parse_error_continuation(input: &str) -> IResult<&str, u64> {
    let (input, _) = space0(input)?;
    let (input, _) =
        alt((tag("Corrected:"), tag("Uncorrectable:"), tag("Unverified:"))).parse(input)?;
    let (input, _) = space0(input)?;
    let (input, count) = parse_u64(input)?;
    Ok((input, count))
}

// ---------------------------------------------------------------------------
// Accumulator
// ---------------------------------------------------------------------------

struct PartialScrub {
    started_at: Option<ScrubTimestamp>,
    status: Option<String>,
    error_count: u64,
    bytes_scrubbed: Option<u64>,
    duration_secs: Option<u64>,
    time_left_secs: Option<u64>,
    eta: Option<ScrubTimestamp>,
    total_bytes: Option<u64>,
    rate_bytes_per_sec: Option<u64>,
}

impl PartialScrub {
    fn new() -> Self {
        Self {
            started_at: None,
            status: None,
            error_count: 0,
            bytes_scrubbed: None,
            duration_secs: None,
            time_left_secs: None,
            eta: None,
            total_bytes: None,
            rate_bytes_per_sec: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parses scrub status while keeping terminal-state classification independent
/// from display timestamp parsing. `Status:` decides terminal states; a missing
/// or unparseable start timestamp becomes `started_at: None`, not `Unknown`.
pub fn parse_btrfs_scrub_status(
    raw: &RawCommandOutput,
) -> Result<BtrfsScrubStatusOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let stdout = &raw.stdout;

    // "no stats available" means scrub has never run
    if stdout.contains("no stats available") {
        return Ok(BtrfsScrubStatusOutput {
            state: ScrubState::Never,
        });
    }

    let mut acc = PartialScrub::new();

    for line in stdout.lines() {
        if let Ok((_, ts)) = parse_scrub_started_or_resumed(line) {
            if !ts.is_empty() && !ts.contains("not available") {
                acc.started_at = parse_ctime(ts).map(ScrubTimestamp);
            }
        } else if let Ok((_, status)) = parse_status_line(line) {
            acc.status = Some(status.to_owned());
        } else if let Ok((_, secs)) = parse_duration_line(line) {
            acc.duration_secs = Some(secs);
        } else if let Ok((_, secs)) = parse_time_left_line(line) {
            acc.time_left_secs = Some(secs);
        } else if let Ok((_, ts)) = parse_eta_line(line) {
            acc.eta = Some(ts);
        } else if let Ok((_, bytes)) = parse_total_to_scrub(line) {
            acc.total_bytes = Some(bytes);
        } else if let Ok((_, bytes)) = parse_bytes_scrubbed(line) {
            acc.bytes_scrubbed = Some(bytes);
        } else if let Ok((_, rate)) = parse_rate_line(line) {
            acc.rate_bytes_per_sec = Some(rate);
        } else if let Ok((_, count)) = parse_error_summary(line) {
            acc.error_count = acc.error_count.saturating_add(count);
        } else if let Ok((_, count)) = parse_error_continuation(line) {
            acc.error_count = acc.error_count.saturating_add(count);
        }
    }

    if acc.status.as_deref() == Some("running") {
        return Ok(BtrfsScrubStatusOutput {
            state: ScrubState::Running {
                started_at: acc.started_at,
                duration_secs: acc.duration_secs,
                time_left_secs: acc.time_left_secs,
                eta: acc.eta,
                total_bytes: acc.total_bytes,
                bytes_scrubbed: acc.bytes_scrubbed,
                rate_bytes_per_sec: acc.rate_bytes_per_sec,
                error_count: acc.error_count,
            },
        });
    }

    let started_at = acc.started_at;
    let state = match acc.status.as_deref() {
        Some("finished") => ScrubState::Finished {
            started_at,
            error_count: acc.error_count,
            duration_secs: acc.duration_secs,
            total_bytes: acc.total_bytes,
            rate_bytes_per_sec: acc.rate_bytes_per_sec,
        },
        Some("aborted") => ScrubState::Aborted {
            started_at,
            error_count: acc.error_count,
            duration_secs: acc.duration_secs,
            total_bytes: acc.total_bytes,
            rate_bytes_per_sec: acc.rate_bytes_per_sec,
        },
        Some("interrupted") => ScrubState::Interrupted {
            started_at,
            error_count: acc.error_count,
            duration_secs: acc.duration_secs,
            total_bytes: acc.total_bytes,
            rate_bytes_per_sec: acc.rate_bytes_per_sec,
        },
        _ => ScrubState::Unknown,
    };

    Ok(BtrfsScrubStatusOutput { state })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::read_stable_fixture as fixture;

    // --- Contract tests (nixos-26.05 fixtures) ---

    /// Intent: Parse a real "never scrubbed" fixture.
    /// Why: Ensures the parser recognizes the "no stats available" sentinel.
    /// Scenario: Pool has never been scrubbed.
    #[test]
    fn scrub_parses_nixos_26_05_never() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status".into(),
            stdout: fixture("btrfs-scrub-never.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        assert_eq!(out.state, ScrubState::Never);
    }

    /// Intent: Parse a real completed-scrub fixture.
    /// Why: Validates timestamp, error count, total bytes, and rate from live output.
    /// Scenario: Scrub completed successfully on a healthy pool.
    #[test]
    fn scrub_parses_nixos_26_05_completed() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: fixture("btrfs-scrub-completed.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        // Extract expected timestamp directly from fixture
        let expected_ts = raw
            .stdout
            .lines()
            .find_map(|l| l.trim().strip_prefix("Scrub started:"))
            .unwrap()
            .trim();
        let expected_dt = parse_ctime(expected_ts).unwrap();
        match &out.state {
            ScrubState::Finished {
                started_at,
                error_count,
                total_bytes,
                rate_bytes_per_sec,
                ..
            } => {
                assert_eq!(started_at.as_ref().map(|ts| ts.0), Some(expected_dt));
                assert_eq!(*error_count, 0);
                assert_eq!(*total_bytes, Some(33_964_032));
                assert_eq!(*rate_bytes_per_sec, Some(33_947_648));
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    /// Intent: Parse a real running-scrub fixture captured from --raw output.
    /// Why: Validates all Running fields against live btrfs output.
    /// Scenario: Mid-scrub status check on a multi-drive pool.
    #[test]
    fn scrub_parses_nixos_26_05_running() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: fixture("btrfs-scrub-running.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Running {
                started_at,
                duration_secs,
                time_left_secs,
                eta,
                total_bytes,
                bytes_scrubbed,
                rate_bytes_per_sec,
                error_count,
            } => {
                assert!(started_at.is_some());
                assert!(duration_secs.is_some());
                assert!(time_left_secs.is_some());
                assert!(eta.is_some());
                assert_eq!(*total_bytes, Some(3_224_780_800));
                assert_eq!(*bytes_scrubbed, Some(2_729_836_544));
                assert_eq!(*rate_bytes_per_sec, Some(545_967_308));
                assert_eq!(*error_count, 0);
            }
            other => panic!("expected Running, got {other:?}"),
        }
    }

    // --- Synthetic tests (inline) ---

    /// Intent: Running scrub with all fields populates the Running variant.
    /// Why: The Running variant now carries duration, time_left, eta, and
    /// bytes_scrubbed -- all must be extracted from --raw output.
    /// Scenario: Mid-scrub status check on a 3-drive pool.
    #[test]
    fn scrub_running_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    Tue Feb 25 10:00:00 2026
Status:           running
Duration:         0:05:58
Time left:        0:34:24
ETA:              Tue Feb 25 10:40:22 2026
Total to scrub:   596353253376
Bytes scrubbed:   88143626240  (14.78%)
Rate:             246211246/s
Error summary:    no errors found
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Running {
                started_at,
                duration_secs,
                time_left_secs,
                eta,
                total_bytes,
                bytes_scrubbed,
                rate_bytes_per_sec,
                error_count,
            } => {
                assert!(started_at.is_some());
                assert_eq!(*duration_secs, Some(358));
                assert_eq!(*time_left_secs, Some(2064));
                assert!(eta.is_some());
                assert_eq!(*total_bytes, Some(596_353_253_376));
                assert_eq!(*bytes_scrubbed, Some(88_143_626_240));
                assert_eq!(*rate_bytes_per_sec, Some(246_211_246));
                assert_eq!(*error_count, 0);
            }
            other => panic!("expected Running, got {other:?}"),
        }
    }

    /// Intent: Running scrub with minimal fields (just Status: running).
    /// Why: All Running fields except error_count are optional -- the parser
    /// must not fail when btrfs emits a sparse output.
    /// Scenario: Very early in a scrub, btrfs hasn't computed estimates yet.
    #[test]
    fn scrub_running_minimal() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Status:           running
Error summary:    no errors found
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Running {
                started_at,
                duration_secs,
                time_left_secs,
                eta,
                total_bytes,
                bytes_scrubbed,
                rate_bytes_per_sec,
                error_count,
            } => {
                assert!(started_at.is_none());
                assert_eq!(*duration_secs, None);
                assert_eq!(*time_left_secs, None);
                assert!(eta.is_none());
                assert_eq!(*total_bytes, None);
                assert_eq!(*bytes_scrubbed, None);
                assert_eq!(*rate_bytes_per_sec, None);
                assert_eq!(*error_count, 0);
            }
            other => panic!("expected Running, got {other:?}"),
        }
    }

    /// Intent: Finished scrub with read + csum errors sums correctly.
    /// Why: Error summary key=val pairs must all be summed into error_count.
    /// Scenario: Scrub found data corruption on a degraded pool.
    #[test]
    fn scrub_finished_with_errors_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    Tue Feb 25 10:00:00 2026
Status:           finished
Duration:         0:00:10
Total to scrub:   1073741824
Rate:             104857600/s
Error summary:    read=1 csum=2
  Corrected:      2
  Uncorrectable:  1
  Unverified:     0
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Finished {
                started_at,
                error_count,
                duration_secs,
                total_bytes,
                rate_bytes_per_sec,
            } => {
                assert_eq!(
                    started_at.as_ref().map(|ts| ts.0),
                    Some(parse_ctime("Tue Feb 25 10:00:00 2026").unwrap())
                );
                // read=1 + csum=2 + Corrected=2 + Uncorrectable=1 + Unverified=0
                assert_eq!(*error_count, 6);
                assert_eq!(*duration_secs, Some(10));
                assert_eq!(*total_bytes, Some(1_073_741_824));
                assert_eq!(*rate_bytes_per_sec, Some(104_857_600));
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    // Intent: scrub error counts saturate when summary and continuation
    // counters exceed u64.
    // Why it exists: btrfs error counters are diagnostic external-tool output,
    // so huge values must not panic, wrap to zero, or suppress error reporting.
    // Scenario: corrupt scrub status output reports u64::MAX read errors plus
    // more counters on the following lines.
    #[test]
    fn scrub_error_count_saturates_on_large_summary_and_continuation() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    Tue Feb 25 10:00:00 2026
Status:           finished
Duration:         0:00:10
Error summary:    read=18446744073709551615 csum=1
  Corrected:      1
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Finished { error_count, .. } => {
                assert_eq!(*error_count, u64::MAX);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    /// Intent: Aborted scrub status gets a distinct terminal state.
    /// Why: Cancelled scrubs are resumable and must not be rendered as finished.
    /// Scenario: braid lock cancels an in-flight scrub before unmounting.
    #[test]
    fn scrub_status_aborted_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    Tue Feb 25 10:00:00 2026
Status:           aborted
Duration:         0:00:10
Total to scrub:   1073741824
Rate:             104857600/s
Error summary:    csum=2
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Aborted {
                started_at,
                error_count,
                duration_secs,
                total_bytes,
                rate_bytes_per_sec,
            } => {
                assert_eq!(
                    started_at.as_ref().map(|ts| ts.0),
                    Some(parse_ctime("Tue Feb 25 10:00:00 2026").unwrap())
                );
                assert_eq!(*error_count, 2);
                assert_eq!(*duration_secs, Some(10));
                assert_eq!(*total_bytes, Some(1_073_741_824));
                assert_eq!(*rate_bytes_per_sec, Some(104_857_600));
            }
            other => panic!("expected Aborted, got {other:?}"),
        }
    }

    /// Intent: Interrupted scrub status gets a distinct terminal state.
    /// Why: A scrub killed without a cancel ioctl is not clean completion and
    /// must not be displayed as finished.
    /// Scenario: host loses power while a scrub userspace process is active.
    #[test]
    fn scrub_status_interrupted_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    Tue Feb 25 10:00:00 2026
Status:           interrupted
Duration:         0:00:10
Total to scrub:   1073741824
Rate:             104857600/s
Error summary:    no errors found
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Interrupted {
                started_at,
                error_count,
                duration_secs,
                total_bytes,
                rate_bytes_per_sec,
            } => {
                assert_eq!(
                    started_at.as_ref().map(|ts| ts.0),
                    Some(parse_ctime("Tue Feb 25 10:00:00 2026").unwrap())
                );
                assert_eq!(*error_count, 0);
                assert_eq!(*duration_secs, Some(10));
                assert_eq!(*total_bytes, Some(1_073_741_824));
                assert_eq!(*rate_bytes_per_sec, Some(104_857_600));
            }
            other => panic!("expected Interrupted, got {other:?}"),
        }
    }

    /// Intent: Unknown terminal status words remain Unknown.
    /// Why: New btrfs status strings must not be silently bucketed into a
    /// known terminal state with different semantics.
    /// Scenario: a future btrfs-progs release adds a scrub status value.
    #[test]
    fn scrub_status_unknown_status_word_is_unknown() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    Tue Feb 25 10:00:00 2026
Status:           weird
Duration:         0:00:10
Error summary:    no errors found
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        assert_eq!(out.state, ScrubState::Unknown);
    }

    #[test]
    // Intent: an aborted scrub missing its start line still classifies as Aborted.
    // Why it exists: terminal state must come from the `Status:` word; a missing
    // start line must not downgrade a resumable state to `Unknown`.
    // Scenario: btrfs emits a sparse terminal block after a cancelled scrub.
    fn scrub_aborted_without_started_is_aborted() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Status:           aborted
Duration:         0:00:10
Total to scrub:   1073741824
Rate:             104857600/s
Error summary:    csum=2
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match out.state {
            ScrubState::Aborted {
                started_at,
                error_count,
                duration_secs,
                total_bytes,
                rate_bytes_per_sec,
            } => {
                assert_eq!(started_at, None);
                assert_eq!(error_count, 2);
                assert_eq!(duration_secs, Some(10));
                assert_eq!(total_bytes, Some(1_073_741_824));
                assert_eq!(rate_bytes_per_sec, Some(104_857_600));
            }
            other => panic!("expected Aborted, got {other:?}"),
        }
    }

    #[test]
    // Intent: an interrupted scrub whose start timestamp fails to parse still
    // classifies as Interrupted.
    // Why it exists: `parse_ctime` is format/locale-fragile; future drift must
    // not flip a resumable state into `Unknown`.
    // Scenario: btrfs prints the start line in an unexpected format.
    fn scrub_interrupted_with_unparseable_started_is_interrupted() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    2026-02-25 10:00:00
Status:           interrupted
Duration:         0:00:10
Total to scrub:   1073741824
Rate:             104857600/s
Error summary:    no errors found
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match out.state {
            ScrubState::Interrupted {
                started_at,
                error_count,
                duration_secs,
                total_bytes,
                rate_bytes_per_sec,
            } => {
                assert_eq!(started_at, None);
                assert_eq!(error_count, 0);
                assert_eq!(duration_secs, Some(10));
                assert_eq!(total_bytes, Some(1_073_741_824));
                assert_eq!(rate_bytes_per_sec, Some(104_857_600));
            }
            other => panic!("expected Interrupted, got {other:?}"),
        }
    }

    #[test]
    // Intent: a finished scrub with no parseable start time still classifies as
    // Finished, not Unknown.
    // Why it exists: completion is authoritative from the `Status:` word; the
    // start timestamp is decoration.
    // Scenario: sparse terminal block on a completed scrub.
    fn scrub_finished_without_started_is_finished() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Status:           finished
Duration:         0:00:10
Total to scrub:   1073741824
Rate:             104857600/s
Error summary:    no errors found
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match out.state {
            ScrubState::Finished {
                started_at,
                error_count,
                duration_secs,
                total_bytes,
                rate_bytes_per_sec,
            } => {
                assert_eq!(started_at, None);
                assert_eq!(error_count, 0);
                assert_eq!(duration_secs, Some(10));
                assert_eq!(total_bytes, Some(1_073_741_824));
                assert_eq!(rate_bytes_per_sec, Some(104_857_600));
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    /// Intent: Rate line with a scrub limit suffix must still parse correctly.
    /// Why: btrfs-progs appends ` (limit <bytes>/s)` when per-device scrub
    /// limits are set (scrub.c lines 216-218). The parser must extract only the
    /// rate number before the first `/`.
    /// Scenario: User has configured a per-device scrub rate limit.
    #[test]
    fn scrub_finished_with_rate_limit() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    Tue Feb 25 10:00:00 2026
Status:           finished
Duration:         0:00:10
Total to scrub:   1073741824
Rate:             104857600/s (limit 52428800/s)
Error summary:    no errors found
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Finished {
                rate_bytes_per_sec, ..
            } => {
                assert_eq!(*rate_bytes_per_sec, Some(104_857_600));
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    /// Intent: Uncorrectable errors on continuation lines are counted even
    /// when the Error summary line has no key=value entries.
    /// Why: btrfs-progs triggers the error block when corrected_errors +
    /// uncorrectable_errors > 0 (err_cnt2), even if read/csum/verify/super
    /// are all zero. Without parsing continuation lines, error_count would
    /// be 0 despite real uncorrectable errors.
    /// Scenario: btrfs corrected all initial errors via RAID copies, but
    /// some remained uncorrectable.
    #[test]
    fn scrub_errors_uncorrectable_only() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    Tue Feb 25 10:00:00 2026
Status:           finished
Duration:         0:00:10
Total to scrub:   1073741824
Rate:             104857600/s
Error summary:
  Corrected:      5
  Uncorrectable:  2
  Unverified:     0
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Finished { error_count, .. } => {
                // Corrected=5 + Uncorrectable=2 + Unverified=0
                assert_eq!(*error_count, 7);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    /// Intent: "Scrub resumed:" is handled the same as "Scrub started:".
    /// Why: btrfs-progs emits "Scrub resumed:" when a scrub was paused and
    /// resumed (scrub.c line 328). The parser must extract the timestamp.
    /// Scenario: User resumed a previously cancelled scrub.
    #[test]
    fn scrub_resumed_parses_timestamp() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub resumed:    Tue Feb 25 12:00:00 2026
Status:           finished
Duration:         0:00:05
Total to scrub:   1073741824
Rate:             104857600/s
Error summary:    no errors found
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Finished { started_at, .. } => {
                assert_eq!(
                    started_at.as_ref().map(|ts| ts.0),
                    Some(parse_ctime("Tue Feb 25 12:00:00 2026").unwrap())
                );
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    /// Intent: Empty output produces Unknown, not a parse error.
    /// Why: Defensive handling for unexpected btrfs output.
    /// Scenario: btrfs returns empty stdout with exit code 0.
    #[test]
    fn scrub_unknown_on_empty_output() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        assert_eq!(out.state, ScrubState::Unknown);
    }
}
