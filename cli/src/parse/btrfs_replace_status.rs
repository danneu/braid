use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::{tag, take_until, take_while_m_n, take_while1},
    character::complete::u64 as parse_u64,
    combinator::{all_consuming, recognize},
    error::{Error, ErrorKind},
};

use crate::cmd::RawCommandOutput;

use super::ParseError;
use super::types::ReplaceState;

fn parse_percent(input: &str) -> IResult<&str, f64> {
    let (input, token) = recognize((
        take_while1(|c: char| c.is_ascii_digit()),
        tag("."),
        take_while_m_n(1, 1, |c: char| c.is_ascii_digit()),
        tag("%"),
    ))
    .parse(input)?;

    let value = token
        .strip_suffix('%')
        .expect("percent parser includes trailing %")
        .parse::<f64>()
        .map_err(|_| nom::Err::Error(Error::new(token, ErrorKind::Float)))?;

    if value.is_finite() && (0.0..=100.0).contains(&value) {
        Ok((input, value))
    } else {
        Err(nom::Err::Error(Error::new(token, ErrorKind::Verify)))
    }
}

fn parse_nonempty_until<'a>(input: &'a str, delimiter: &str) -> IResult<&'a str, &'a str> {
    let original = input;
    let (input, value) = take_until(delimiter)(input)?;
    if value.is_empty() {
        return Err(nom::Err::Error(Error::new(original, ErrorKind::Verify)));
    }
    Ok((input, value))
}

fn parse_error_counters(input: &str) -> IResult<&str, ()> {
    let (input, _) = tag(", ")(input)?;
    let (input, _) = parse_u64(input)?;
    let (input, _) = tag(" write errs, ")(input)?;
    let (input, _) = parse_u64(input)?;
    let (input, _) = tag(" uncorr. read errs")(input)?;
    Ok((input, ()))
}

fn parse_running(input: &str) -> IResult<&str, ReplaceState> {
    let (input, pct) = parse_percent(input)?;
    let (input, _) = tag(" done")(input)?;
    let (input, _) = parse_error_counters(input)?;
    Ok((input, ReplaceState::Running { pct }))
}

fn parse_finished(input: &str) -> IResult<&str, ReplaceState> {
    let (input, _) = tag("Started on ")(input)?;
    let (input, _) = parse_nonempty_until(input, ", finished on ")?;
    let (input, _) = tag(", finished on ")(input)?;
    let (input, _) = parse_nonempty_until(input, ", ")?;
    let (input, _) = parse_error_counters(input)?;
    Ok((input, ReplaceState::Finished))
}

fn parse_cancelled(input: &str) -> IResult<&str, ReplaceState> {
    let (input, _) = tag("Started on ")(input)?;
    let (input, _) = parse_nonempty_until(input, ", canceled on ")?;
    let (input, _) = tag(", canceled on ")(input)?;
    let (input, _) = parse_nonempty_until(input, " at ")?;
    let (input, _) = tag(" at ")(input)?;
    let (input, _) = parse_percent(input)?;
    let (input, _) = parse_error_counters(input)?;
    Ok((input, ReplaceState::Cancelled))
}

fn parse_suspended(input: &str) -> IResult<&str, ReplaceState> {
    let (input, _) = tag("Started on ")(input)?;
    let (input, _) = parse_nonempty_until(input, ", suspended on ")?;
    let (input, _) = tag(", suspended on ")(input)?;
    let (input, _) = parse_nonempty_until(input, " at ")?;
    let (input, _) = tag(" at ")(input)?;
    let (input, pct) = parse_percent(input)?;
    let (input, _) = parse_error_counters(input)?;
    Ok((input, ReplaceState::Suspended { pct }))
}

fn parse_never_started(input: &str) -> IResult<&str, ReplaceState> {
    let (input, _) = tag("Never started")(input)?;
    Ok((input, ReplaceState::NotStarted))
}

fn parse_replace_status_line(input: &str) -> IResult<&str, ReplaceState> {
    all_consuming(alt((
        parse_running,
        parse_finished,
        parse_cancelled,
        parse_suspended,
        parse_never_started,
    )))
    .parse(input)
}

/// Parse the output of `btrfs replace status -1 <mount_point>`.
///
/// Possible outputs (per reference/btrfs-progs/cmds/replace.c:451-505):
/// - Running: `"45.3% done, 0 write errs, 0 uncorr. read errs"`
/// - Finished: `"Started on <t1>, finished on <t2>, 0 write errs, 0 uncorr. read errs"`
/// - Canceled: `"Started on <t1>, canceled on <t2> at 0.0%, ..."`
/// - Suspended: `"Started on <t1>, suspended on <t2> at 12.5%, ..."`
/// - Never started: `"Never started"`
///
/// The zero-exit stdout must trim to exactly one recognised status line. Any
/// other text returns `Err(ParseError::InvalidText)` because upstream
/// `btrfs-progs` emits one of the strings above for every success-exit case.
pub fn parse_btrfs_replace_status(raw: &RawCommandOutput) -> Result<ReplaceState, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let stdout = &raw.stdout;

    parse_replace_status_line(stdout.trim())
        .map(|(_, state)| state)
        .map_err(|_| ParseError::InvalidText {
            cmd: raw.cmd.clone(),
            detail: format!("unrecognised btrfs replace status output: {stdout:?}"),
        })
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

    fn assert_invalid_text(stdout: &str) {
        let err = parse_btrfs_replace_status(&raw(stdout)).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidText { .. }),
            "expected InvalidText for {stdout:?}, got {err:?}"
        );
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
    // Intent: "Never started" output maps to the exact idle state emitted by
    // btrfs-progs.
    // Why it exists: btrfs-progs replace.c:486-490 emits this exact output
    // with skip_stats = 1, so no percentage or error counters follow it.
    // Scenario: a fixture capture checks replace status before any replace
    // has ever been issued on the filesystem.
    fn never_started() {
        let out = parse_btrfs_replace_status(&raw("Never started\n")).unwrap();
        assert_eq!(out, ReplaceState::NotStarted);
    }

    #[test]
    // Intent: suspended output without a percentage is rejected.
    // Why it exists: upstream includes progress2string for every suspended
    // status, so missing progress means the rendered grammar drifted.
    // Scenario: btrfs-progs keeps the suspended state word but omits the
    // "at NN.N%" fragment.
    fn suspended_no_percent_token_returns_err() {
        assert_invalid_text(
            "Started on 27.Feb 10:30:00, suspended on 27.Feb 10:35:00, 0 write errs, 0 uncorr. read errs\n",
        );
    }

    #[test]
    // Intent: empty zero-exit stdout is rejected as a parser-contract
    //   violation rather than silently coerced to NotStarted.
    // Why it exists: upstream btrfs-progs (reference/btrfs-progs/cmds/replace.c:450-505)
    //   never prints empty stdout on a zero exit; any zero-exit output that
    //   doesn't match a recognised prefix means we cannot reason about kernel
    //   replace state, and callers like wait_for_kernel_replace_to_finish must
    //   fail closed instead of treating it as "nothing to do".
    // Scenario: a hypothetical environment where the command prints nothing on
    //   stdout and exits 0; the parser refuses to invent a state for it.
    fn empty_stdout_returns_err() {
        let err = parse_btrfs_replace_status(&raw("")).unwrap_err();
        assert!(
            matches!(err, ParseError::InvalidText { .. }),
            "expected InvalidText, got {err:?}"
        );
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
    // Intent: unrecognised zero-exit stdout returns InvalidText carrying the
    //   offending bytes, instead of silently bucketing it as NotStarted.
    // Why it exists: a future upstream wording change (e.g. "% done" rendered
    //   as "% complete") would otherwise make wait_for_kernel_replace_to_finish
    //   exit at the first poll, racing the kernel resume worker and clearing
    //   the journal -- the exact regression commit b551555 was added to
    //   prevent. A loud parse error makes the drift visible in tests and at
    //   runtime.
    // Scenario: a fictional reworded line lands on stdout with a zero exit
    //   and the parser refuses to classify it.
    fn garbage_output_returns_err() {
        let err = parse_btrfs_replace_status(&raw("something unexpected here\n")).unwrap_err();
        match err {
            ParseError::InvalidText { detail, .. } => {
                assert!(
                    detail.contains("something unexpected"),
                    "detail should echo offending bytes, got: {detail}"
                );
            }
            other => panic!("expected InvalidText, got {other:?}"),
        }
    }

    #[test]
    // Intent: a zero-exit "no operation running" line is rejected.
    // Why it exists: current btrfs-progs emits "Never started" for the idle
    // state; accepting stale wording would hide output-contract drift.
    // Scenario: an older or fictional btrfs-progs build returns an idle phrase
    // that braid no longer treats as authoritative.
    fn no_operation_running_returns_err() {
        assert_invalid_text("no operation running\n");
    }

    #[test]
    // Intent: the running percentage accepts only progress2string syntax.
    // Why it exists: Rust float parsing accepts signs, exponent notation, and
    // special values that upstream cannot produce.
    // Scenario: malformed status text includes a percent-like token and must
    // fail closed instead of being normalized into progress.
    fn invalid_percent_forms_return_err() {
        let cases = [
            "-1.0% done, 0 write errs, 0 uncorr. read errs\n",
            "+1.0% done, 0 write errs, 0 uncorr. read errs\n",
            "1% done, 0 write errs, 0 uncorr. read errs\n",
            "1.23% done, 0 write errs, 0 uncorr. read errs\n",
            "1.% done, 0 write errs, 0 uncorr. read errs\n",
            "1.0e1% done, 0 write errs, 0 uncorr. read errs\n",
            "NaN% done, 0 write errs, 0 uncorr. read errs\n",
            "inf% done, 0 write errs, 0 uncorr. read errs\n",
            "100.1% done, 0 write errs, 0 uncorr. read errs\n",
        ];

        for case in cases {
            assert_invalid_text(case);
        }
    }

    #[test]
    // Intent: status lines must be complete, single-line, and fully anchored.
    // Why it exists: substring parsing previously accepted prefixes, embedded
    // extra lines, and counterless output that upstream does not produce.
    // Scenario: a future output change preserves a familiar fragment but drops
    // required context that callers need for fail-closed classification.
    fn rejects_partial_multiline_and_counterless_output() {
        let cases = [
            "prefix Never started\n",
            "Never started\njunk\n",
            "45.3% done\n",
            "45.3% done, 0 write errs\n",
            "45.3% done, 0 write errs, 0 uncorr. read errs extra\n",
            "Started on 27.Feb 10:30:00, finished on 27.Feb 10:35:00\n",
            "Started on 27.Feb 10:30:00, canceled on 27.Feb 10:35:00 at 0.0%\n",
        ];

        for case in cases {
            assert_invalid_text(case);
        }
    }

    #[test]
    // Intent: non-zero exit from btrfs replace status must be a parse error
    //   that preserves the full diagnostic payload (cmd, exit_code, stderr).
    // Why: the success-exit path classifies stdout strictly; non-zero exits
    //   must surface as CommandFailed rather than reaching that classifier.
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
