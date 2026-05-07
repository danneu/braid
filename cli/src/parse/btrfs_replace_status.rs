use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::ReplaceState;

/// Parse the output of `btrfs replace status -1 <mount_point>`.
///
/// Possible outputs (per reference/btrfs-progs/cmds/replace.c:451-505):
/// - Running:  "45.3% done, 0 write errs, 0 uncorr. read errs"
/// - Finished: "Started on <t1>, finished on <t2>, 0 write errs, 0 uncorr. read errs"
/// - Canceled: "Started on <t1>, canceled on <t2> at 0.0%, ..."
/// - Suspended: "Started on <t1>, suspended on <t2> at 12.5%, ..."
/// - Never started: "Never started"
/// - Not running: "no operation running" or empty stdout
pub fn parse_btrfs_replace_status(raw: &RawCommandOutput) -> Result<ReplaceState, ParseError> {
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
        return Ok(ReplaceState::Finished);
    }

    if stdout.contains("canceled on") {
        return Ok(ReplaceState::Cancelled);
    }

    if stdout.contains("suspended on") {
        return Ok(ReplaceState::Suspended {
            pct: extract_percent(stdout).unwrap_or(0.0),
        });
    }

    if stdout.contains("Never started") {
        return Ok(ReplaceState::NotStarted);
    }

    // Look for percentage: "45.3% done" or "100.0% done".
    if stdout.contains("% done")
        && let Some(pct) = extract_percent(stdout)
    {
        return Ok(ReplaceState::Running { pct });
    }

    // No operation running (or unrecognised output -- treat as not started).
    Ok(ReplaceState::NotStarted)
}

fn extract_percent(text: &str) -> Option<f64> {
    // Match an "NN.N%" token anywhere in the output.
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(pos) = trimmed.find('%') {
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
    // Intent: parse the real upstream STARTED-state output of
    //   `btrfs replace status -1`.
    // Why: replace.c:451-505 prints "<pct>% done, <n> write errs, <n> uncorr.
    //   read errs" — there is no "Started on …, running, pid: …" prefix in
    //   the running state. Earlier versions of this test used a fictional
    //   prefix that production never produces, masking the fact that the
    //   `Running { pct }` branch was untested against real bytes.
    fn running_with_percentage() {
        let out =
            parse_btrfs_replace_status(&raw("45.3% done, 0 write errs, 0 uncorr. read errs\n"))
                .unwrap();
        match out {
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
        assert_eq!(out, ReplaceState::Finished);
    }

    #[test]
    // Intent: canceled btrfs replace output maps to the kernel-canceled state
    // without carrying the rendered zero percent.
    // Why it exists: btrfs-progs replace.c:466-475 emits the canceled row,
    // while the kernel always renders CANCELED with progress_1000 = 0
    // (dev-replace.c:1051-1054), so callers should not infer progress.
    // Scenario: an interrupted replacement was manually canceled by the
    // operator or kernel and recovery must route it through rollback cleanup.
    fn canceled_zero_percent() {
        let out = parse_btrfs_replace_status(&raw(
            "Started on 27.Feb 10:30:00, canceled on 27.Feb 10:35:00 at 0.0%, 0 write errs, 0 uncorr. read errs\n",
        ))
        .unwrap();
        assert_eq!(out, ReplaceState::Cancelled);
    }

    #[test]
    // Intent: suspended btrfs replace output maps to Suspended with its real
    // progress percentage.
    // Why it exists: btrfs-progs replace.c:476-485 emits "suspended on ... at
    // NN.N%" and the kernel can enter this state when the target disappears
    // (dev-replace.c:1200-1251); recovery must distinguish it from idle.
    // Scenario: a replacement target became unavailable during shutdown and
    // the operator needs an explicit manual-cancel recovery path.
    fn suspended_with_percentage() {
        let out = parse_btrfs_replace_status(&raw(
            "Started on 27.Feb 10:30:00, suspended on 27.Feb 10:35:00 at 12.5%, 0 write errs, 0 uncorr. read errs\n",
        ))
        .unwrap();
        match out {
            ReplaceState::Suspended { pct } => {
                assert!((pct - 12.5).abs() < 0.01, "expected 12.5, got {pct}");
            }
            other => panic!("expected Suspended, got {other:?}"),
        }
    }

    #[test]
    // Intent: "Never started" output maps to the lenient NotStarted bucket.
    // Why it exists: btrfs-progs replace.c:486-490 emits this exact output
    // with skip_stats = 1, so no percentage or error counters follow it.
    // Scenario: a fixture capture checks replace status before any replace
    // has ever been issued on the filesystem.
    fn never_started() {
        let out = parse_btrfs_replace_status(&raw("Never started\n")).unwrap();
        assert_eq!(out, ReplaceState::NotStarted);
    }

    #[test]
    // Intent: suspended output without a percent token still maps to
    // Suspended with a defensive zero progress value.
    // Why it exists: the prefix is more important than the rendering detail;
    // future btrfs-progs formatting drift should not collapse a suspended
    // kernel operation into NotStarted.
    // Scenario: status text keeps the suspended state word but omits the
    // "at NN.N%" fragment.
    fn suspended_no_percent_token_falls_back_to_zero() {
        let out = parse_btrfs_replace_status(&raw(
            "Started on 27.Feb 10:30:00, suspended on 27.Feb 10:35:00, 0 write errs, 0 uncorr. read errs\n",
        ))
        .unwrap();
        assert_eq!(out, ReplaceState::Suspended { pct: 0.0 });
    }

    #[test]
    fn not_started() {
        let out = parse_btrfs_replace_status(&raw("")).unwrap();
        assert_eq!(out, ReplaceState::NotStarted);
    }

    #[test]
    fn no_operation_running() {
        let out = parse_btrfs_replace_status(&raw("no operation running\n")).unwrap();
        assert_eq!(out, ReplaceState::NotStarted);
    }

    #[test]
    // Intent: parse the real upstream STARTED-state output at the upper bound.
    // Why: the kernel reports progress as a per-mille count (replace.c via
    //   `progress2string`); 1000/1000 renders as "100.0% done". Same fictional
    //   prefix removal as `running_with_percentage`.
    fn running_100_percent() {
        let out =
            parse_btrfs_replace_status(&raw("100.0% done, 0 write errs, 0 uncorr. read errs\n"))
                .unwrap();
        match out {
            ReplaceState::Running { pct } => {
                assert!((pct - 100.0).abs() < 0.01, "expected 100.0, got {pct}");
            }
            other => panic!("expected Running, got {other:?}"),
        }
    }

    #[test]
    fn garbage_output_treated_as_not_started() {
        let out = parse_btrfs_replace_status(&raw("something unexpected here\n")).unwrap();
        assert_eq!(out, ReplaceState::NotStarted);
    }

    #[test]
    // Intent: non-zero exit from btrfs replace status must be a parse error
    //   that preserves the full diagnostic payload (cmd, exit_code, stderr).
    // Why: the parser's lenient fallback treats unrecognised successful output
    //   as NotStarted, so non-zero exits must bypass that path.
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
