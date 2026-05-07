# Tighten `parse_btrfs_replace_status` and surface its errors in recover

## Context

`cli/src/parse/btrfs_replace_status.rs:45-53` falls through to `Ok(ReplaceState::NotStarted)` for any zero-exit stdout that doesn't match one of the five recognised prefixes. The doc comment also lists `"no operation running"` and empty stdout as legitimate inputs, but upstream `btrfs-progs` emits neither (`reference/btrfs-progs/cmds/replace.c:450-505` only ever prints one of: `"<pct>% done, ..."`, `"Started on ..., finished on ..."`, `"Started on ..., canceled on ... at <pct>%, ..."`, `"Started on ..., suspended on ... at <pct>%, ..."`, or `"Never started"`; the kernel-error path goes to stderr with exit `-EINVAL`).

Two consequences:

1. The sibling parser `parse_btrfs_balance_status` (`cli/src/parse/btrfs_balance_status.rs:142-154`) already returns `Err(ParseError::InvalidText)` on unrecognised text and on empty stdout. The replace parser is the lone outlier with a silent fallback.
2. `recover.rs::wait_for_kernel_replace_to_finish` (`cli/src/recover.rs:2713-2806`) treats `NotStarted` as terminal and returns `Ok(())` without emitting anything to the operator. So if a future `btrfs-progs` rewords `"% done"` to e.g. `"% complete"`, the wait loop exits at the first poll, the `relock_and_remount` cycle races the kernel `dev_replace` resume worker, downstream replace recovery writes `pool.json` and clears `pending-op.json`, and the operator sees nothing — the exact regression the function was added to prevent (commit `b551555`).

`progress.rs::run_replace_with_progress` (`cli/src/progress.rs:357-397`) is robust either way: its loop exits on `handle.is_finished()`, not on the parse result, and `Err` and `NotStarted` both fall through the same `continue` arm. The `replace_progress_poll_parse_failure_is_silent` test (`cli/src/progress.rs:1006-1034`) already pins this.

The change tightens the parser and makes `recover.rs` fail closed (preserving the journal) on parser-contract failures, so a future upstream wording change is loud at runtime and at unit-test time. Subprocess/runner failures stay best-effort but become operator-visible.

## Changes

### 1. Parser (`cli/src/parse/btrfs_replace_status.rs`)

Update the function body:

- Drop the `// No operation running (or unrecognised output -- treat as not started).` arm. Replace with `Err(ParseError::InvalidText { cmd: raw.cmd.clone(), detail: format!("unrecognised btrfs replace status output: {stdout:?}") })`.
- Reuse the existing `ParseError::InvalidText` variant from `cli/src/parse/mod.rs:47-48` (no new variant; matches `parse_btrfs_balance_status` convention at lines 100, 118, 151).
- Update the doc comment at lines 6-14 to drop the "no operation running" / "empty stdout" claims and state that any other zero-exit stdout is `Err(InvalidText)`.

### 2. Caller (`cli/src/recover.rs`)

In `wait_for_kernel_replace_to_finish` (lines 2713-2806), split the two error paths so they have different shapes:

- Lines 2728-2737 (`runner.run` Err — subprocess/transient failure): drop the `if wait_emitted` gate so the `[warn]` line fires on first-poll failures too. Still `return Ok(())`. Subprocess failures can be transient (a brief race, ENOMEM, signal) and are not by themselves evidence that the kernel is mid-replace.
- Lines 2741-2750 (`parse_btrfs_replace_status` Err — contract failure): change to `[fail]` + `return Err(RecoverError::Failed(...))`. Mirror the existing `Suspended` arm (lines 2771-2777): emit a fail row that names the unrecognised output and tells the operator to upgrade braid (or report drift), then return `RecoverError::Failed`, which preserves `pending-op.json` so the next `braid recover` can retry. This is the load-bearing change: parse failures mean we cannot reason about kernel replace state, so proceeding into `relock_and_remount` and downstream replace-recovery (which writes `pool.json` and clears `pending-op.json`) re-opens the exact race the wait was added to close.

Rationale for the split: the `Suspended` arm already establishes the "fail closed and preserve the journal when state is unknown or unsafe" precedent. `InvalidText` is the same kind of "we don't know what's going on" — treat it identically. The subprocess-failure arm stays best-effort because a transient runner Err on a never-replaced pool would otherwise force-fail every recover; the docstring's "relock_and_remount and probe_pool will catch any remaining staleness as a clear test failure rather than a hang" backstop applies there.

The fail message should match the existing `Suspended` style and tell the operator something actionable. Suggested text:
`format!("pool: kernel dev_replace status returned unrecognised output (preserving journal; report upstream wording change). Re-run `braid recover` after upgrading braid. stdout: {stdout:?}")`
where `stdout` is `raw.stdout.clone()` -- the original bytes the parser was given, taken directly from the successful `runner.run` result. (Going through the parser error's `detail` would re-wrap the same bytes with the parser's `"unrecognised btrfs replace status output: "` prefix.)

No change to `progress.rs`.

### 3. Tests

All new and materially rewritten tests must carry the repo's `// Intent` / `// Why it exists` / `// Scenario` preamble (`docs/testing.md`; existing examples at `cli/src/parse/btrfs_replace_status.rs:88-95, 117-124, 133-140, 154-159`).

**`cli/src/parse/btrfs_replace_status.rs`:**

- Rename and rewrite `garbage_output_treated_as_not_started` (lines 211-214) to `garbage_output_returns_err`. Assert `matches!(err, ParseError::InvalidText { .. })` and that `detail` mentions the bytes (`assert!(detail.contains("something unexpected"))`). Add the three-line preamble.
- Delete `no_operation_running` (lines 188-191): upstream never emits that string.
- Rename `not_started` (lines 182-185, empty stdout) to `empty_stdout_returns_err` and rewrite to assert `Err(InvalidText)` rather than `Ok(NotStarted)`. (Empty stdout on zero exit cannot occur per upstream; keep the test as a contract pin in the new direction. The original name would mislead readers about what the test now asserts.) Add the three-line preamble.

**`cli/src/recover.rs`:**

Add three tests in the existing `mod tests` block (next to `wait_for_kernel_replace_emits_warn_on_status_error_after_wait` at line 3603), each with the `// Intent` / `// Why it exists` / `// Scenario` preamble:

- `wait_for_kernel_replace_emits_warn_on_status_error_first_poll`: runner returns `Err` on the very first poll. Assert single captured line `"[warn] pool: kernel dev_replace status check failed -- proceeding"` and `Ok(())` return. Pins the "subprocess failure is best-effort" branch of the split.
- `wait_for_kernel_replace_emits_fail_on_unrecognised_stdout_first_poll`: runner returns `ok_raw("btrfs replace status -1 /mnt/storage", "75.0% complete, 0 write errs, 0 uncorr. read errs\n")` (a fictional reworded line). Assert single captured `[fail]` row, `RecoverError::Failed` return, and that the error message contains the offending stdout substring. This is the exact regression the finding cares about; pinning it as a test makes a future upstream wording change a clear test failure rather than a silent skip.
- `wait_for_kernel_replace_emits_fail_on_unrecognised_stdout_after_wait`: runner returns one running poll (`"5.0% done, ..."`), then the unrecognised reworded line. Assert the captured rows are `[wait] ... waiting for kernel dev_replace to finish...`, `... 5.0%`, `[fail] ... unrecognised output ...`, and `RecoverError::Failed` return. Closes the announced wait window with the fail row.

Reuse the existing `ReplaceStatusSequenceRunner` and `ok_raw` / `err_raw` helpers in the same `mod tests`.

**`cli/src/progress.rs`:**

No change — `replace_progress_poll_parse_failure_is_silent` (line 1006) still passes because the `if let Ok(...) && let Ok(status) = ...` short-circuit makes `Err` and `NotStarted` indistinguishable in this caller.

### 4. Files modified

- `cli/src/parse/btrfs_replace_status.rs` — parser body, doc comment, three test updates (each with the `// Intent` / `// Why it exists` / `// Scenario` preamble).
- `cli/src/recover.rs` — drop `if wait_emitted` gate on the runner-Err arm; convert the parse-Err arm to `[fail]` + `RecoverError::Failed`; add three tests (each with the preamble).

No fixture changes (the three replace fixtures at `cli/tests/fixtures/nixos-25.11/btrfs-replace-status-{canceled,finished,never-started}.txt` and the matching unstable mirrors stay valid; they exercise recognised states).

## Verification

1. `just test-rust` — covers parser unit tests, golden contract tests, and the new `recover.rs` `mod tests` cases.
2. `just test-vm recover-replace-not-started recover-replace-completed` — exercises the recover flow end-to-end against real `btrfs replace status` output for the `Never started` and `Finished` arms (`tests/cli/recover-replace-not-started.nix`, `tests/cli/recover-replace-completed.nix`, registered in `flake.nix:430-438`).
3. `just test-vm ups-lb-during-replace` — exercises the kernel-resumed replace path that drives `wait_for_kernel_replace_to_finish` against a real running replace (`tests/module/ups-lb-during-replace.nix`).

The unstable lane (`just test-rust-unstable`, `just test-all-unstable`) requires no new fixtures — its existing replace fixtures cover canceled/finished/never-started, which remain `Ok` paths.
