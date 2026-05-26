# Pin human-mode query-failed output in the UPS status VM canary

## Context

`braid ups status` has a 2x5 behavioral matrix: `{human, json}` output x
`{success, empty-status, query-failed, invocation-failed, not-enabled}`
outcomes. The VM canary `tests/cli/braid-status-ups.py` pins **9 of 10**
cells explicitly. The lone gap is **human query-failed** -- exactly the
most operator-visible failure (upsd is down, operator runs plain
`braid ups status`).

This is the only end-to-end path that proves `UpsError::QueryFailed`
renders to **stderr** (not stdout), with the `error: upsc query failed:`
prefix, an empty stdout, and a non-zero exit -- as wired through
`main.rs`, not just the typed error in isolation.

Coverage today:
- `cli/src/ups.rs:709` (`cmd_ups_status_non_zero_exit_is_query_failed`)
  pins the typed `UpsError::QueryFailed` + its `Display` string, but
  calls `cmd_ups_status` directly -- it never exercises the `main.rs`
  arm that routes the error to stderr and sets the exit code.
- `braid-status-ups.py:79-101` pins **JSON** query-failed (stdout JSON
  sentinel, silent stderr).
- `braid-status-ups.py:159-179` pins **human invocation-failed** (the
  rarer "nut package/PATH broken" case) end-to-end.

The `main.rs` human-error arm (`main.rs:1007-1010`:
`Err(e) => { print_cli_error(&e.to_string()); exit(1); }`) is shared by
both `QueryFailed` and `InvocationFailed` -- main.rs does not branch on
the variant. So the *primary* justification for this test is matrix
completeness and pinning the common operator path, not novel
regression-catching (a stdout-swap already fails the `.expect_err` Rust
test; an exit-to-zero already fails the invocation-failed VM assert on
the same shared arm). The new test is a cheap, behavioral, end-to-end
pin of the one cell that was left open.

Intended outcome: the human query-failed path is pinned end-to-end at
near-zero incremental VM cost, closing the matrix.

## Change

Single file: `tests/cli/braid-status-ups.py`. No Rust or module changes;
no new test file (this is a branch within an existing VM test script, so
it follows the file's `# --- Section ---` header style, not the
Intent/Why/Scenario preamble reserved for top-level test functions).

Insert a **human query-failed** branch immediately after the JSON
query-failed block (after the `assert err == ""` block that currently
ends at line 101, before the `# --- Invocation-failed branch ---` header
at line 103). This reuses the already-stopped `upsd.service` (stopped at
line 79, never restarted), so there is no extra setup.

Mirror the structure of the human invocation-failed sibling
(`braid-status-ups.py:159-179`): tolerant `machine.execute` to `/tmp`
files, then assert against `cat`-ed output.

Assertions:
- exit code `!= 0`
- stdout (`/tmp/ups_qf_human.out`) is exactly empty
- stderr (`/tmp/ups_qf_human.err`) `.startswith("error: upsc query failed:")`
  -- the `print_cli_error` `error: ` prefix (`main.rs:1259-1265`) +
  `UpsError::QueryFailed` Display
- stderr contains `"Connection failure"` -- the stable live-upsc-stderr
  slice, identical to the substring the JSON block already asserts at
  line 96 (use `contains`, not an exact match, because the full upsc
  wording varies)
- stderr does **not** contain `"invocation failed"` -- symmetric
  cross-check mirroring the invocation-failed block's
  `assert "upsc query failed" not in err_if_human` (line 177), guarding
  against variant confusion

Use fresh temp paths (`/tmp/ups_qf_human.out` / `.err`) to avoid
clobbering the JSON block's `/tmp/ups_qf.out` / `.err`.

### Sketch (to be placed after line 101)

```python
# --- Query-failed branch, human mode ---
# Same stopped-upsd state as the JSON block above, now without --json.
# The common "my UPS daemon is down" failure must land on stderr with the
# query-failed prefix, leave stdout empty, and exit non-zero. This is the
# lone uncovered cell of the {human,json} x {outcome} matrix: JSON
# query-failed is pinned above and human invocation-failed below, but
# human query-failed -- the most operator-visible failure, and the path
# preflight cross-references -- was unpinned.
exit_code = machine.execute(
    "braid ups status >/tmp/ups_qf_human.out 2>/tmp/ups_qf_human.err"
)[0]
assert exit_code != 0, (
    "braid ups status must exit non-zero when query fails; got 0"
)
out_qf_human = machine.succeed("cat /tmp/ups_qf_human.out")
err_qf_human = machine.succeed("cat /tmp/ups_qf_human.err")
assert out_qf_human == "", (
    f"expected empty stdout in human query-failed, got: {out_qf_human!r}"
)
assert err_qf_human.startswith("error: upsc query failed:"), (
    f"expected human query-failed prefix, got: {err_qf_human!r}"
)
assert "Connection failure" in err_qf_human, (
    f"expected upsc stderr 'Connection failure' in human query-failed, "
    f"got: {err_qf_human!r}"
)
assert "invocation failed" not in err_qf_human, (
    f"invocation-failed wording leaked into human query-failed: "
    f"{err_qf_human!r}"
)
```

## Critical files

- `tests/cli/braid-status-ups.py` -- the only file changed; insert after
  line 101.
- Reference (read-only, no change): `cli/src/ups.rs:124-185`
  (`cmd_ups_status`, `emit_query_failed`), `cli/src/main.rs:1007-1010`
  (shared human-error arm), `cli/src/main.rs:1259-1265`
  (`print_cli_error` prefix).

## Verification

This VM test is registered in `flake.nix` checks. Run the single check:

```
just test-vm braid-status-ups
```

Expected: passes. If the run surfaces an unexpected stderr shape, inspect
with `-v`:

```
just test-vm braid-status-ups -v
```

To confirm the new branch actually fails when the contract is broken
(sanity check that it is not a no-op assert), temporarily verify the
prefix string matches: the human invocation-failed sibling already proves
`print_cli_error` emits the `error: ` prefix over the VM, and the JSON
block already proves `"Connection failure"` appears in live upsc stderr
with upsd stopped -- so both halves of the new assert are independently
demonstrated elsewhere in the same file.

Scope: localized test-only change; no need for the full VM suite. Do not
run `cargo fmt` or any formatter.
