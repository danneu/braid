use nom::{
    IResult,
    bytes::complete::tag,
    character::complete::{not_line_ending, space0},
};
use time::PrimitiveDateTime;
use time::macros::format_description;

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::{BtrfsScrubStatusOutput, ScrubState, ScrubTimestamp};

pub(super) fn parse_ctime(s: &str) -> Option<PrimitiveDateTime> {
    // "Tue Feb 24 02:00:07 2026" — ctime format from btrfs scrub status
    let fmt = format_description!(
        "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]"
    );
    PrimitiveDateTime::parse(s, &fmt).ok()
}

// ---------------------------------------------------------------------------
// nom parsers
// ---------------------------------------------------------------------------

// Parses: "Scrub started:    Tue Feb 24 02:00:07 2026"  →  "Tue Feb 24 02:00:07 2026"
fn parse_scrub_started(input: &str) -> IResult<&str, &str> {
    let (input, _) = space0(input)?;
    let (input, _) = tag("Scrub started:")(input)?;
    let (input, _) = space0(input)?;
    let (input, timestamp) = not_line_ending(input)?;
    Ok((input, timestamp.trim()))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

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

    // Look for key fields
    let mut started_at = None;
    let mut is_running = false;
    let mut error_count: u64 = 0;
    let mut pct: Option<u8> = None;
    let mut duration: Option<String> = None;
    let mut total_bytes: Option<u64> = None;
    let mut rate_bytes_per_sec: Option<u64> = None;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Ok((_, ts)) = parse_scrub_started(line) {
            if !ts.is_empty() && !ts.contains("not available") {
                started_at = Some(ts.to_owned());
            }
        } else if let Some(status) = trimmed.strip_prefix("Status:") {
            is_running = status.trim() == "running";
        } else if let Some(rest) = trimmed.strip_prefix("Error summary:") {
            let rest = rest.trim();
            if rest != "no errors found" {
                // e.g. "csum=3" or "read=1 csum=2"
                error_count = rest
                    .split_whitespace()
                    .filter_map(|kv| kv.split('=').nth(1))
                    .filter_map(|v| v.parse::<u64>().ok())
                    .sum();
            }
        } else if let Some(rest) = trimmed.strip_prefix("Duration:") {
            duration = Some(rest.trim().to_owned());
        } else if let Some(rest) = trimmed.strip_prefix("Total to scrub:") {
            total_bytes = rest.trim().parse::<u64>().ok();
        } else if let Some(rest) = trimmed.strip_prefix("Rate:") {
            // --raw output: "33910682/s" or "33910682/s (limit 52428800/s)"
            // Split on '/' to extract the rate number before the unit suffix.
            rate_bytes_per_sec = rest
                .trim()
                .split('/')
                .next()
                .and_then(|s| s.trim().parse::<u64>().ok());
        } else if trimmed.ends_with("% done") {
            // e.g. "  8.00% done" on some versions, or embedded in other lines
            if let Some(pct_str) = trimmed.split('%').next() {
                pct = pct_str
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|v| (0.0..=100.0).contains(v))
                    // Truncate towards zero; 99.5% shouldn't round to 100%
                    .map(|v| v as u8);
            }
        }
    }

    if is_running {
        return Ok(BtrfsScrubStatusOutput {
            state: ScrubState::Running {
                pct,
                total_bytes,
                rate_bytes_per_sec,
            },
        });
    }

    if let Some(ts) = started_at.and_then(|s| parse_ctime(&s)) {
        return Ok(BtrfsScrubStatusOutput {
            state: ScrubState::Completed {
                started_at: ScrubTimestamp(ts),
                error_count,
                duration,
                total_bytes,
                rate_bytes_per_sec,
            },
        });
    }

    Ok(BtrfsScrubStatusOutput {
        state: ScrubState::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/nixos-25.11/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    // --- Contract tests (nixos-25.11 fixtures) ---

    #[test]
    fn scrub_parses_nixos_25_11_never() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status".into(),
            stdout: fixture("btrfs-scrub-never.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        assert_eq!(out.state, ScrubState::Never);
    }

    #[test]
    fn scrub_parses_nixos_25_11_completed() {
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
            ScrubState::Completed {
                started_at,
                error_count,
                total_bytes,
                rate_bytes_per_sec,
                ..
            } => {
                assert_eq!(started_at.0, expected_dt);
                assert_eq!(*error_count, 0);
                assert_eq!(*total_bytes, Some(33_931_264));
                assert_eq!(*rate_bytes_per_sec, Some(33_914_880));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    // --- Synthetic tests (inline) ---

    #[test]
    fn scrub_running_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status --raw".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    Tue Feb 25 10:00:00 2026
Status:           running
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
        match out.state {
            ScrubState::Running {
                pct,
                total_bytes,
                rate_bytes_per_sec,
            } => {
                assert_eq!(pct, None);
                assert_eq!(total_bytes, Some(1_073_741_824));
                assert_eq!(rate_bytes_per_sec, Some(104_857_600));
            }
            other => panic!("expected Running, got {other:?}"),
        }
    }

    #[test]
    fn scrub_completed_with_errors_inline() {
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
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        match &out.state {
            ScrubState::Completed {
                started_at,
                error_count,
                total_bytes,
                rate_bytes_per_sec,
                ..
            } => {
                assert_eq!(
                    started_at.0,
                    parse_ctime("Tue Feb 25 10:00:00 2026").unwrap()
                );
                assert_eq!(*error_count, 3);
                assert_eq!(*total_bytes, Some(1_073_741_824));
                assert_eq!(*rate_bytes_per_sec, Some(104_857_600));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// Intent: Rate line with a scrub limit suffix must still parse correctly.
    /// Why: btrfs-progs appends ` (limit <bytes>/s)` when per-device scrub
    /// limits are set (scrub.c lines 216-218). The parser must extract only the
    /// rate number before the first `/`.
    /// Scenario: user has configured a per-device scrub rate limit.
    #[test]
    fn scrub_completed_with_rate_limit() {
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
            ScrubState::Completed {
                rate_bytes_per_sec, ..
            } => {
                assert_eq!(*rate_bytes_per_sec, Some(104_857_600));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

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
