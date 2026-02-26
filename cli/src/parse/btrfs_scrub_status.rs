use nom::{
    bytes::complete::tag,
    character::complete::{not_line_ending, space0},
    IResult,
};
use time::macros::format_description;
use time::PrimitiveDateTime;

use crate::cmd::RawCommandOutput;

use super::types::{BtrfsScrubStatusOutput, ScrubState, ScrubTimestamp};
use super::ParseError;

fn parse_ctime(s: &str) -> Option<PrimitiveDateTime> {
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
    let mut total: Option<String> = None;
    let mut rate: Option<String> = None;

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
            total = Some(rest.trim().to_owned());
        } else if let Some(rest) = trimmed.strip_prefix("Rate:") {
            rate = Some(rest.trim().to_owned());
        } else if trimmed.ends_with("% done") {
            // e.g. "  8.00% done" on some versions, or embedded in other lines
            if let Some(pct_str) = trimmed.split('%').next() {
                pct = pct_str.trim().parse::<f64>().ok().map(|v| v as u8);
            }
        }
    }

    if is_running {
        return Ok(BtrfsScrubStatusOutput {
            state: ScrubState::Running { pct, total, rate },
        });
    }

    if let Some(ts) = started_at.and_then(|s| parse_ctime(&s)) {
        return Ok(BtrfsScrubStatusOutput {
            state: ScrubState::Completed {
                started_at: ScrubTimestamp(ts),
                error_count,
                duration,
                total,
                rate,
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
            cmd: "btrfs scrub status".into(),
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
                ..
            } => {
                assert_eq!(started_at.0, expected_dt);
                assert_eq!(*error_count, 0);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    // --- Synthetic tests (inline) ---

    #[test]
    fn scrub_running_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    Tue Feb 25 10:00:00 2026
Status:           running
Duration:         0:00:05
Total to scrub:   1.00GiB
Rate:             100.00MiB/s
Error summary:    no errors found
"
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_scrub_status(&raw).unwrap();
        assert!(matches!(out.state, ScrubState::Running { pct: None, .. }));
    }

    #[test]
    fn scrub_completed_with_errors_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs scrub status".into(),
            stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Scrub started:    Tue Feb 25 10:00:00 2026
Status:           finished
Duration:         0:00:10
Total to scrub:   1.00GiB
Rate:             100.00MiB/s
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
                ..
            } => {
                assert_eq!(
                    started_at.0,
                    parse_ctime("Tue Feb 25 10:00:00 2026").unwrap()
                );
                assert_eq!(*error_count, 3);
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
