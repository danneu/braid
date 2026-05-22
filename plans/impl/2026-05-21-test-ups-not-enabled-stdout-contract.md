# Plan: pin the human not-enabled hint to stdout (exit 0, empty stderr)

## Context

`braid ups status` on a host without UPS configured is documented as
"not an error" (`cli/src/ups.rs:5`, `manual/commands/ups-status.md:8-9`).
The human branch prints a 4-line hint via `println!` (stdout, exit 0)
that talks the operator into editing their NixOS config. The contract
that this hint lands on stdout -- not stderr -- is what makes
`braid ups status > log.txt` capture the helpful message.

A verify-issue review (low severity, testing) flagged that this
contract has zero coverage:

- `tests/cli/braid-status-ups.py:181-189` (the "Not-enabled branch")
  exercises only `--json`.
- `cli/src/ups.rs` unit tests cover `print_not_enabled(json=true)`
  (`json_not_enabled_has_sentinel_error`, `snapshot_json_not_enabled`)
  but never the `json=false` branch.
- A refactor that switched the hint to stderr would silently break
  shell pipelines and no test would fail.

The verify-issue recommendation was a pivot from the originally
proposed Rust unit test (which can only pin wording, not the stream).
The right place is the existing VM test, which already establishes
exactly the split-stream pattern this case needs three sections above
the gap (human invocation-failed at
`tests/cli/braid-status-ups.py:159-179`).

Outcome: a small VM test extension that pins the stream direction,
exit code, and a stable substring of the hint, in the existing not-
enabled section, reusing the `/tmp/no-ups.json` config the section
already materializes.

## Change

Single file: `tests/cli/braid-status-ups.py`.

After line 189 (the existing `--json` not-enabled assertion), append a
human-mode check against the same `/tmp/no-ups.json` config. Match the
split-stream idiom used at lines 159-179:

```python
# Human mode against the same no-ups config: the enable hint must land
# on stdout (so `braid ups status > log.txt` captures it) with empty
# stderr and exit 0. Substring is stable; full wording lives in
# print_not_enabled and is intentionally not snapshotted here.
exit_code = machine.execute(
    "braid --config /tmp/no-ups.json ups status "
    ">/tmp/no_ups_human.out 2>/tmp/no_ups_human.err"
)[0]
assert exit_code == 0, (
    f"braid ups status (no ups configured) must exit 0; got {exit_code}"
)
out_no_ups = machine.succeed("cat /tmp/no_ups_human.out")
err_no_ups = machine.succeed("cat /tmp/no_ups_human.err")
assert "braid.ups.enable = true" in out_no_ups, (
    f"expected enable-hint substring on stdout, got: {out_no_ups!r}"
)
assert err_no_ups == "", (
    f"expected empty stderr in human not-enabled, got: {err_no_ups!r}"
)
```

Notes on shape:

- Reuses the `/tmp/no-ups.json` already created by line 184; no
  additional `jq`/config setup.
- Uses `machine.execute(...)` not `machine.succeed(...)` so the
  exit-code check is explicit and symmetric with the invocation-
  failed human block at lines 159-179.
- Substring `braid.ups.enable = true` is the stable hook -- it is the
  load-bearing instruction in the hint, also referenced as the
  documented config invariant at `manual/commands/ups-status.md:8`
  and dozens of other locations. Wording around it (paragraph
  structure, line wrapping) is allowed to drift without forcing a
  test edit.
- The stderr-empty assertion mirrors the same invariant the
  `--json` branches already pin (lines 99-101, 155-157), so a future
  refactor that flips this stream direction fails the same way it
  would for the `--json` paths.

## Files

- `tests/cli/braid-status-ups.py` -- append ~14 lines after line 189.

No production code changes. No `cli/src/ups.rs` refactor. No new
Rust unit test. No docs change (the existing manual already covers
the contract at a behavioral level).

## Verification

```sh
just test-vm braid-status-ups
```

Expected:

- All existing assertions in the file continue to pass (online, JSON,
  empty-status warning, query-failed JSON, invocation-failed JSON
  and human, not-enabled JSON).
- The new human not-enabled block passes: exit 0, hint substring on
  stdout, empty stderr.

Optional pre-flight sanity (no VM required) to confirm the substring
choice matches what `print_not_enabled` actually emits:

```sh
grep -n 'braid.ups.enable = true' cli/src/ups.rs
```

Should match the `println!` body at `cli/src/ups.rs:148`.

## Non-goals

- Not refactoring `print_not_enabled` to return a `String` parallel to
  `format_human`. The unit-testable shape would not pin the stream,
  which is the contract this gap exposes.
- Not adding an insta snapshot of the hint wording. Snapshots are
  appropriate for the curated `format_human` output (already
  snapshotted across five fixtures) where wording stability is the
  contract; here the contract is stream direction plus exit code,
  and a substring check states that without locking the prose.
- Not extending this to other commands. The verify-issue protocol
  was scoped to this finding; a broader sweep of human-vs-stderr
  contracts across the CLI is a separate audit.
