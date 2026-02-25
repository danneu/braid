use nom::{
    bytes::complete::{tag, take_till1},
    character::complete::{space0, space1, u64 as parse_u64, u8 as parse_u8},
    IResult,
};

use crate::cmd::RawCommandOutput;

use super::types::{BalanceState, BtrfsBalanceStatusOutput};
use super::ParseError;

// ---------------------------------------------------------------------------
// nom parsers
// ---------------------------------------------------------------------------

// Parses chunk-progress line:
//   "3 out of about 10 chunks balanced (7 considered), 70% left"
//   → (3, 10, 7, 70)
fn parse_chunks_line(input: &str) -> IResult<&str, (u64, u64, u64, u8)> {
    let (input, _) = space0(input)?;
    let (input, done) = parse_u64(input)?;
    let (input, _) = space1(input)?;
    let (input, _) = tag("out of about")(input)?;
    let (input, _) = space1(input)?;
    let (input, total) = parse_u64(input)?;
    let (input, _) = space1(input)?;
    let (input, _) = tag("chunks balanced (")(input)?;
    let (input, considered) = parse_u64(input)?;
    let (input, _) = space1(input)?;
    let (input, _) = tag("considered),")(input)?;
    let (input, _) = space1(input)?;
    let (input, pct) = parse_u8(input)?;
    let (input, _) = tag("% left")(input)?;
    Ok((input, (done, total, considered, pct)))
}

// Parses the state line, returning "running" or "paused":
//   "Balance on '/mnt/storage' is running"
//   "Balance on '/mnt/storage' is paused"
fn parse_state_line(input: &str) -> IResult<&str, &str> {
    let (input, _) = tag("Balance on '")(input)?;
    let (input, _) = take_till1(|c| c == '\'')(input)?;
    let (input, _) = tag("' is ")(input)?;
    let (input, state) = take_till1(|c: char| c == ',' || c == '\n' || c.is_ascii_whitespace())(input)?;
    Ok((input, state))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn parse_btrfs_balance_status(
    raw: &RawCommandOutput,
) -> Result<BtrfsBalanceStatusOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let stdout = &raw.stdout;

    // Check for "no balance" first
    if stdout.lines().any(|l| l.contains("No balance found")) {
        return Ok(BtrfsBalanceStatusOutput {
            state: BalanceState::None,
        });
    }

    // Determine running vs paused from the state line
    let state_str = stdout
        .lines()
        .find_map(|l| parse_state_line(l.trim()).ok().map(|(_, s)| s))
        .ok_or_else(|| ParseError::InvalidText {
            cmd: raw.cmd.clone(),
            detail: "no recognizable balance state pattern found".into(),
        })?;

    let is_running = state_str == "running";
    let is_paused = state_str == "paused";

    if !is_running && !is_paused {
        return Err(ParseError::InvalidText {
            cmd: raw.cmd.clone(),
            detail: format!("unexpected balance state: {state_str:?}"),
        });
    }

    // Find chunk progress line
    let (done_chunks, estimated_total_chunks, considered_chunks, pct_left) = stdout
        .lines()
        .find_map(|l| parse_chunks_line(l.trim()).ok().map(|(_, v)| v))
        .ok_or_else(|| ParseError::InvalidText {
            cmd: raw.cmd.clone(),
            detail: "balance running/paused but no chunk progress line found".into(),
        })?;

    let state = if is_running {
        BalanceState::Running {
            done_chunks,
            estimated_total_chunks,
            considered_chunks,
            pct_left,
        }
    } else {
        BalanceState::Paused {
            done_chunks,
            estimated_total_chunks,
            considered_chunks,
            pct_left,
        }
    };

    Ok(BtrfsBalanceStatusOutput { state })
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
    fn balance_status_parses_nixos_25_11_none() {
        let raw = RawCommandOutput {
            cmd: "btrfs balance status".into(),
            stdout: fixture("btrfs-balance-status-none.txt"),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_balance_status(&raw).unwrap();
        assert_eq!(out.state, BalanceState::None);
    }

    // --- Synthetic tests (inline) ---

    #[test]
    fn balance_status_running() {
        let raw = RawCommandOutput {
            cmd: "btrfs balance status".into(),
            stdout: "Balance on '/mnt/storage' is running\n\
                     3 out of about 10 chunks balanced (7 considered), 70% left\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_balance_status(&raw).unwrap();
        assert_eq!(
            out.state,
            BalanceState::Running {
                done_chunks: 3,
                estimated_total_chunks: 10,
                considered_chunks: 7,
                pct_left: 70,
            }
        );
    }

    #[test]
    fn balance_status_paused() {
        let raw = RawCommandOutput {
            cmd: "btrfs balance status".into(),
            stdout: "Balance on '/mnt/storage' is paused\n\
                     5 out of about 12 chunks balanced (8 considered), 58% left\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_balance_status(&raw).unwrap();
        assert_eq!(
            out.state,
            BalanceState::Paused {
                done_chunks: 5,
                estimated_total_chunks: 12,
                considered_chunks: 8,
                pct_left: 58,
            }
        );
    }

    #[test]
    fn balance_status_running_with_extra_lines() {
        let raw = RawCommandOutput {
            cmd: "btrfs balance status".into(),
            stdout: "WARNING: some diagnostic message\n\
                     Balance on '/mnt/storage' is running, send cancel command to interrupt\n\
                     expected:\n\
                     2 out of about 6 chunks balanced (4 considered), 66% left\n\
                     extra trailing info\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_balance_status(&raw).unwrap();
        assert_eq!(
            out.state,
            BalanceState::Running {
                done_chunks: 2,
                estimated_total_chunks: 6,
                considered_chunks: 4,
                pct_left: 66,
            }
        );
    }

    #[test]
    fn balance_status_no_balance_inline() {
        let raw = RawCommandOutput {
            cmd: "btrfs balance status".into(),
            stdout: "No balance found on '/mnt/storage'\n".into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let out = parse_btrfs_balance_status(&raw).unwrap();
        assert_eq!(out.state, BalanceState::None);
    }

    #[test]
    fn balance_status_error_exit() {
        let raw = RawCommandOutput {
            cmd: "btrfs balance status".into(),
            stdout: String::new(),
            stderr: "ERROR: not a btrfs filesystem".into(),
            exit_status: 1,
        };
        let err = parse_btrfs_balance_status(&raw).unwrap_err();
        assert!(matches!(err, ParseError::CommandFailed { .. }));
    }

    #[test]
    fn balance_status_empty_output() {
        let raw = RawCommandOutput {
            cmd: "btrfs balance status".into(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        };
        let err = parse_btrfs_balance_status(&raw).unwrap_err();
        assert!(matches!(err, ParseError::InvalidText { .. }));
    }
}
