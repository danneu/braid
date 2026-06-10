# Pin the degraded-refused exit-code contract in `braid recover` VM test

## Context

`docs/commands/recover.md` documents a two-sided exit-code contract for
`braid recover`:

> Without `--allow-degraded`, refuses to mount if devices are missing
> (exit code 2 for degraded-refused, **distinguishing it from other errors**).

The word "distinguishing" makes this a two-part promise: a degraded refusal
exits `2`, and every *other* failure exits something else (the generic `1`).
The mapping that delivers it lives in the `Commands::Recover` dispatch arm in
`cli/src/main.rs` -- `RecoverError::Mount(MountError::DegradedRefused(_))`
calls `std::process::exit(2)`, while the catch-all `Err(e)` arm calls
`std::process::exit(1)`.

That arm is a bare `std::process::exit`, so it is **not** reachable by any
in-process Rust unit test (the call terminates the process). The Rust tests in
`cli/src/recover.rs` only pin that `cmd_recover` *returns* the `DegradedRefused`
variant; they cannot see the variant-to-exit-code mapping. The subprocess/VM
lane is therefore the only place this contract can be pinned -- and it currently
asserts only `exit_code != 0` for the degraded path and nothing at all for the
"other errors" half. A refactor that folded `DegradedRefused` into the generic
`exit(1)` arm (or that collapsed every error onto `exit(2)`) would pass the
whole suite while silently breaking the documented contract.

This is the lone gap: the sibling `unlock` command has the identical
`DegradedRefused -> exit(2)` arm and it is already pinned at `== 2` in two VM
tests -- `tests/cli/braid-unlock.py` (Test 7) and
`tests/cli/braid-unlock-key-file.py` (Test 2b, which even carries a "Why it
exists" note describing this exact regression). This change brings
`braid-recover.py` into line with that established house pattern.

**Outcome:** both halves of the documented contract are pinned at the VM lane
where the dispatch mapping actually runs.

## Changes

All edits are in **`tests/cli/braid-recover.py`** -- no source or doc changes
(the code and docs are already correct; the test is the only gap).

### 1. Test 3a (degraded refusal) -- pin exit `2`

In the subtest
`"Test 3a: dry-run preserved-context failure -> stdout empty, stderr has context"`,
tighten the post-`machine.execute` assertion:

- From: `assert exit_code != 0, f"recover --dry-run should refuse without --allow-degraded, got exit {exit_code}"`
- To:   `assert exit_code == 2, f"recover --dry-run degraded refusal must exit 2 (distinct from generic errors), got exit {exit_code}"`

This subtest already drives the exact dispatch path: it injects a journal whose
`target_membership` names an absent `disk3`, so `plan_open_pool` returns
`DegradedRefused`, `cmd_recover` propagates it, and `main.rs` maps it to
`exit(2)`. (`--dry-run` does not change the arm taken -- the match is on the
error variant, and the subtest already asserts the refusal text reaches stderr.)

Update the trailing line of that subtest's `# Scenario:` comment block so the
prose matches the assertion: change "...with stdout empty and a nonzero exit
code." to "...with stdout empty and exit code 2 (the degraded-refused code,
distinct from the generic exit 1)."

### 2. Test 3c (no-journal failure) -- pin exit `1`

In the subtest
`"Test 3c: no-journal failure -> stdout empty, stderr has only the error"`,
tighten the assertion that today only checks for nonzero:

- From: `assert exit_code != 0, f"recover --dry-run with no journal should fail, got exit {exit_code}"`
- To:   `assert exit_code == 1, f"no-journal failure must exit 1 (generic), not the degraded-refused 2, got exit {exit_code}"`

This pins the *other* half of "distinguishing": a non-degraded failure
(missing `pending-op.json`) takes the catch-all `Err(e) => exit(1)` arm, never
the `exit(2)` arm. The subtest already moves `pending-op.json` aside and
captures `exit_code` via `machine.execute`, so this is a one-line tightening
with no new setup.

Add one sentence to that subtest's `# Scenario:` comment block noting the exit
code is the generic `1`, not the degraded-refused `2` -- i.e. this is the
complement that proves exit `2` is reserved for degraded refusals.

## Out of scope (deliberately left as `!= 0`)

- The `"braid unlock refuses with journal present"` subtest's `assert
  exit_code != 0`. That exercises the pending-op preflight gate, a different
  contract from degraded-refused; tightening it would be unrelated scope creep.

No shared helper is extracted: the two `exit(2)` arms in `main.rs` (`unlock`
and `recover`) sit in different command branches over different error enums
(`UnlockError` vs `RecoverError`), and the duplication is two readable lines.
Pinning each command's contract at its own VM lane is the correct shape.

## Verification

Run the VM test (registered as the `braid-recover` check at `flake.nix:604`,
which reads `braid-recover.py` via `tests/cli/braid-recover.nix`):

```
just test-vm braid-recover
```

Expected: the full script passes. Both tightened assertions hold against the
current source, because `recover` already exits `2` on degraded refusal and `1`
on the missing-journal error -- this change pins existing-correct behavior, so
no source edit is needed to make it green.

**Optional teeth check** (prove the new assertions can actually fail): the
strongest is `== 2` over `!= 0` by construction. To confirm end-to-end, one can
temporarily edit the `DegradedRefused` arm in `cli/src/main.rs` to `exit(1)` and
re-run `just test-vm braid-recover` -- Test 3a must now fail on the `== 2`
assertion. Revert the source edit afterward; it is not part of this change.
