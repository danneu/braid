use crate::cmd::RawCommandOutput;

use super::types::{BtrfsReplaceStatusOutput, ReplaceState};
use super::ParseError;

/// Parse the output of `btrfs replace status <mount_point>`.
///
/// Possible outputs:
/// - Running: "Started on ...  45.3% done, ..."
/// - Finished: "Started on ... finished on ..."
/// - Not running: "no operation running" or empty stdout
pub fn parse_btrfs_replace_status(
    raw: &RawCommandOutput,
) -> Result<BtrfsReplaceStatusOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let stdout = &raw.stdout;

    // "finished on" indicates completion
    if stdout.contains("finished on") {
        return Ok(BtrfsReplaceStatusOutput {
            state: ReplaceState::Finished,
        });
    }

    // Look for percentage: "45.3% done" or "100.0% done"
    if let Some(pct) = extract_percent(stdout) {
        return Ok(BtrfsReplaceStatusOutput {
            state: ReplaceState::Running { pct },
        });
    }

    // No operation running (or unrecognised output — treat as none)
    Ok(BtrfsReplaceStatusOutput {
        state: ReplaceState::None,
    })
}

fn extract_percent(text: &str) -> Option<f64> {
    // Match "NN.N% done" anywhere in the output
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(pos) = trimmed.find("% done") {
            // Walk backwards from the '%' to find the start of the number
            let before = &trimmed[..pos];
            let num_start = before
                .rfind(|c: char| !c.is_ascii_digit() && c != '.')
                .map(|i| i + 1)
                .unwrap_or(0);
            if let Ok(pct) = before[num_start..].parse::<f64>() {
                return Some(pct);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "btrfs replace status".into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    #[test]
    fn running_with_percentage() {
        let out = parse_btrfs_replace_status(&raw(
            "Started on 27.Feb 10:30:00, running, pid: 1234, 45.3% done, 0 write errs, 0 uncorr. read errs\n",
        ))
        .unwrap();
        match out.state {
            ReplaceState::Running { pct } => {
                assert!((pct - 45.3).abs() < 0.01, "expected 45.3, got {pct}");
            }
            other => panic!("expected Running, got {other:?}"),
        }
    }

    #[test]
    fn finished() {
        let out = parse_btrfs_replace_status(&raw(
            "Started on 27.Feb 10:30:00, finished on 27.Feb 10:35:00, 0 write errs, 0 uncorr. read errs\n",
        ))
        .unwrap();
        assert_eq!(out.state, ReplaceState::Finished);
    }

    #[test]
    fn not_started() {
        let out = parse_btrfs_replace_status(&raw("")).unwrap();
        assert_eq!(out.state, ReplaceState::None);
    }

    #[test]
    fn no_operation_running() {
        let out = parse_btrfs_replace_status(&raw("no operation running\n")).unwrap();
        assert_eq!(out.state, ReplaceState::None);
    }

    #[test]
    fn running_100_percent() {
        let out = parse_btrfs_replace_status(&raw(
            "Started on 27.Feb 10:30:00, running, pid: 5678, 100.0% done, 0 write errs, 0 uncorr. read errs\n",
        ))
        .unwrap();
        match out.state {
            ReplaceState::Running { pct } => {
                assert!((pct - 100.0).abs() < 0.01, "expected 100.0, got {pct}");
            }
            other => panic!("expected Running, got {other:?}"),
        }
    }

    #[test]
    fn garbage_output_treated_as_none() {
        let out = parse_btrfs_replace_status(&raw("something unexpected here\n")).unwrap();
        assert_eq!(out.state, ReplaceState::None);
    }

    #[test]
    // Intent: non-zero exit from btrfs replace status must be a parse error
    //   that preserves the full diagnostic payload (cmd, exit_code, stderr).
    // Why: the parser previously fell through to Ok(ReplaceState::None) for any
    //   unrecognised output, silently masking command failures.
    // Scenario: typo in mount path → btrfs exits 1 with empty stdout.
    fn nonzero_exit_is_error() {
        let result = parse_btrfs_replace_status(&RawCommandOutput {
            cmd: "btrfs replace status /mnt/storage".into(),
            stdout: String::new(),
            stderr: "ERROR: not a btrfs filesystem: /mnt/stoarge".into(),
            exit_status: 1,
        });
        match result.unwrap_err() {
            ParseError::CommandFailed {
                cmd,
                exit_code,
                stderr,
            } => {
                assert_eq!(cmd, "btrfs replace status /mnt/storage");
                assert_eq!(exit_code, 1);
                assert_eq!(stderr, "ERROR: not a btrfs filesystem: /mnt/stoarge");
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    #[test]
    // Intent: non-zero exit takes precedence even when stdout contains text.
    // Why: a command can write partial output before failing; the exit code is
    //   the authoritative success/failure signal.
    // Scenario: btrfs replace status writes garbage to stdout but exits non-zero.
    fn nonzero_exit_with_garbage_stdout_is_error() {
        let result = parse_btrfs_replace_status(&RawCommandOutput {
            cmd: "btrfs replace status /mnt/storage".into(),
            stdout: "something unexpected here\n".into(),
            stderr: "some error".into(),
            exit_status: 1,
        });
        match result.unwrap_err() {
            ParseError::CommandFailed {
                cmd,
                exit_code,
                stderr,
            } => {
                assert_eq!(cmd, "btrfs replace status /mnt/storage");
                assert_eq!(exit_code, 1);
                assert_eq!(stderr, "some error");
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }
}
