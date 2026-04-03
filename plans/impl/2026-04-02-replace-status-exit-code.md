# Fix: btrfs replace status parser ignores exit codes

## Context

`parse_btrfs_replace_status` (`cli/src/parse/btrfs_replace_status.rs:12-34`) never
checks `raw.exit_status`. When the command fails (bad mount path, permissions, etc.),
the parser falls through to `Ok(ReplaceState::None)` — silently reporting "no replace
in progress" instead of propagating the error.

Every other parser in `cli/src/parse/` guards with
`if raw.exit_status != 0 { return Err(ParseError::CommandFailed { ... }) }`.
This parser is the only one missing the check.

### Upstream behavior (reference/btrfs-progs/cmds/replace.c:379-410)

`btrfs replace status` exits 0 for **all** normal states (started, finished, canceled,
suspended, never_started) and exits non-zero only on errors (bad path, ioctl failure).
This is simpler than `btrfs balance status` (which exits 1 for running/paused) — we can
check exit code first, before content matching.

### Safety impact: idle/autosuspend

`idle.rs:82-84` — `cmd_idle` calls `parse_btrfs_replace_status` and propagates via `?`.
Today, a failed `btrfs replace status` command returns `Ok(ReplaceState::None)`, so
`cmd_idle` concludes no replace is running and returns `Idle` — potentially allowing
autosuspend during an active replace. After the fix, `cmd_idle` will propagate the
`ParseError::CommandFailed` as an `IdleError`, failing closed.

### Not in scope: progress.rs

The progress poller (`progress.rs:229-251`) also calls `parse_btrfs_replace_status`, but
its silent-failure behavior is intentional and already tested
(`replace_progress_poll_parse_failure_is_silent` at line 534). The poller is display-only;
the actual replace outcome comes from the main command thread at line 257
(`handle.join()`). After the parser fix, a non-zero-exit poll will return `Err` instead of
`Ok(None)`, but the `if let Ok(...) && let Ok(...)` guard already handles that by skipping
the block — same observable behavior, no change needed.

## Plan

### 1. Add failing tests

**Parser tests** — `cli/src/parse/btrfs_replace_status.rs`, two new tests asserting the
specific error variant:

Note: `ParseError` does not derive `PartialEq`, so we destructure with `match` instead
of `assert_eq!`. The existing codebase (`cryptsetup_luks_label.rs:121`) only checks
`matches!(err, ParseError::CommandFailed { .. })` — these new tests are stricter,
verifying all three forwarded fields.

```rust
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
        ParseError::CommandFailed { cmd, exit_code, stderr } => {
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
        ParseError::CommandFailed { cmd, exit_code, stderr } => {
            assert_eq!(cmd, "btrfs replace status /mnt/storage");
            assert_eq!(exit_code, 1);
            assert_eq!(stderr, "some error");
        }
        other => panic!("expected CommandFailed, got {other:?}"),
    }
}
```

**Idle regression test** — `cli/src/idle.rs`, one new test proving `cmd_idle` returns
an error (not `Idle`) when the replace status command fails. Uses the existing
`MockRunner` and fixture helpers already in the idle test module:

```rust
#[test]
// Intent: replace status command fails → cmd_idle returns IdleError::Parse(CommandFailed),
//   not Idle.
// Why: a failed status check must not be mistaken for "no replace running" —
//   that would allow autosuspend during an active replace.
// Scenario: typo in mount path causes btrfs replace status to exit non-zero.
fn replace_status_failure_is_not_idle() {
    let (fmnt_req, fmnt_out) = findmnt_mounted();
    let (scrub_req, scrub_out) = scrub_completed();
    let (bal_req, bal_out) = balance_none();

    let runner = MockRunner::default()
        .with_output(fmnt_req, fmnt_out)
        .with_output(scrub_req, scrub_out)
        .with_output(bal_req, bal_out)
        .with_output(
            CmdRequest::BtrfsReplaceStatus {
                mount_point: MountPoint(MP.to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs replace status /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: not a btrfs filesystem".into(),
                exit_status: 1,
            },
        );

    let result = cmd_idle(&runner, MP);
    let err = result.unwrap_err();
    assert!(
        matches!(err, IdleError::Parse(ParseError::CommandFailed { exit_code: 1, .. })),
        "expected IdleError::Parse(CommandFailed {{ exit_code: 1 }}), got {err:?}"
    );
}
```

Run `just test-rust` — all three new tests fail (parser returns `Ok(ReplaceState::None)`).

### 2. Add exit-code guard

`cli/src/parse/btrfs_replace_status.rs:14` — insert at top of
`parse_btrfs_replace_status`, before any content matching:

```rust
if raw.exit_status != 0 {
    return Err(ParseError::CommandFailed {
        cmd: raw.cmd.clone(),
        exit_code: raw.exit_status,
        stderr: raw.stderr.clone(),
    });
}
```

Unlike `btrfs_balance_status` (which must parse stdout before checking exit code because
balance uses exit 1 for running/paused), replace status exits 0 for all normal states,
so the guard goes first unconditionally.

### 3. Run tests

`just test-rust` — all three new tests pass, all existing tests still pass (existing
tests all use `exit_status: 0`).

## Files modified

- `cli/src/parse/btrfs_replace_status.rs` — add exit-code guard + 2 parser tests
- `cli/src/idle.rs` — add 1 regression test

## Verification

```
just test-rust
```
