use crate::cmd::RawCommandOutput;

use super::types::{BtrfsScrubStatusOutput, ScrubState, ScrubTimestamp};
use super::ParseError;

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

    // Look for "Scrub started:" line with a timestamp
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(timestamp) = trimmed.strip_prefix("Scrub started:") {
            let ts = timestamp.trim();
            if !ts.is_empty() && !ts.contains("not available") {
                return Ok(BtrfsScrubStatusOutput {
                    state: ScrubState::Completed {
                        started_at: ScrubTimestamp(ts.to_owned()),
                    },
                });
            }
        }
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
        let expected_ts = raw.stdout.lines()
            .find_map(|l| l.trim().strip_prefix("Scrub started:"))
            .unwrap().trim();
        match &out.state {
            ScrubState::Completed { started_at } => {
                assert_eq!(started_at.0, expected_ts);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    // --- Synthetic tests (inline) ---

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
