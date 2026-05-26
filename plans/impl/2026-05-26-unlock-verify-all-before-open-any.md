# Plan: regression test pinning "verify all to-unlock disks before opening any mapper"

## Context

`braid unlock`/`mount` must verify the credential against *every* to-unlock
disk's slot 0 *before* opening any LUKS mapper (Principle 4,
`docs/design/principles.md:34`: "Before any irreversible operation, every
reachable existing LUKS device ... has its slot 0 verified"). This guarantee is
implemented in `open_disks_with_credential` (`cli/src/mount.rs:508-615`): a
verify-all loop (`verify_credential_for_targets`, line 518) that early-returns on
any rejection, followed by a separate open-all loop (line 552). If a regression
collapsed those two loops into "verify disk1, then open everything," a later
disk whose slot 0 diverged would no longer block the opens -- reintroducing
partial mapper opens.

That ordering is currently **unprotected by tests**:

- `passphrase_mismatch_names_failing_disk` (`cli/src/unlock.rs:452`) wires disk2
  to fail at *both* verify and open, so it still passes under the regression --
  disk2's failure simply resurfaces at the open step (exit 2), and the message
  at `mount.rs:575-588` still names disk2 and not disk1.
- `wrong_passphrase_zero_open_cleanup_is_noop` (`cli/src/mount.rs:2970`) *does*
  assert zero opens, but wires the **first** disk (disk1) to reject. The
  regression keeps disk1's verify, so this test can't catch it.
- Helper-level tests in `cli/src/credential_verify.rs` pin
  `verify_credential_for_targets`'s verify-all-in-order / stop-at-first-rejection
  behavior, but involve **no mapper opens** -- they don't pin the *composition*
  in `open_disks_with_credential`.

The gap is precisely: a **non-first** disk's verify rejection must prevent **any**
mapper open. Outcome: one focused regression test that fails if the verify-all
step is ever reordered after (or interleaved with) the open loop.

### Scope

Unlock/mount path only. The sibling Principle 4 callsites are deliberately out
of scope: `replace` already guards this (`cli/src/replace.rs:4255-4268`: verify
exactly once + "must not trigger CryptsetupLuksOpen"), and `recover`
(`cli/src/recover.rs:2147, 2841`), `add`, and `enroll` factor verification into a
standalone preflight that returns before any mutation -- a lower-risk
composition enforced by ordinary `?` control flow, not two interleaved loops in
one function.

## Change

Add **one** Rust unit test to `cli/src/mount.rs`'s `mod tests`, placed
immediately after `wrong_passphrase_zero_open_cleanup_is_noop` (line 2970) as
its non-first-disk sibling. Suggested name:
`non_first_disk_verify_rejection_opens_no_mapper`.

It mirrors `wrong_passphrase_zero_open_cleanup_is_noop` exactly, shifting the
rejecting disk from disk1 to disk2 and adding the ordering assertions. Drive
`execute_unlock_and_mount` directly (not `cmd_unlock`) so the test sits one layer
below planning and has direct access to the `UnlockAndMountFailure`
(`opened_mappers`).

### Test conventions preamble (required by AGENTS.md)

```
// Intent: a verify rejection on a NON-first to-unlock disk must prevent any
//   mapper open -- the credential is verify-tested against every disk before
//   the open loop begins.
// Why it exists: passphrase_mismatch_names_failing_disk and
//   wrong_passphrase_zero_open_cleanup_is_noop both still pass if verification
//   regresses to "verify disk1, then open all", because disk2's failure then
//   resurfaces at the open step. This pins the Principle 4 "verify all before
//   open any" ordering against that reordering.
// Scenario: 2-disk RAID1 where someone changed disk2's passphrase outside
//   braid; disk2's header is intact, disk1 still accepts.
```

### Runner / fixtures (all exist -- reuse, do not add new helpers)

- `direct_two_disk_plan()` -- `to_unlock` is `[disk1, disk2]` in order, so disk2
  is a genuine non-first disk (`cli/src/test_fixtures/mount.rs:320`).
- `direct_two_disk_fs_with_mappers()`, `test_config()`, `test_passphrase()`
  (yields `b"testpass"` == `MOUNT_TEST_PASSPHRASE_BYTES`),
  `mock_virtio_backing_path_resolver()`.
- Build the `MockRunner` by hand (like the sibling), seeding only:
  - disk1 `CryptsetupTestPassphrase` => ok, via
    `.with_output_stdin(req, MOUNT_TEST_PASSPHRASE_BYTES.to_vec(), ok_raw(...))`
    (first disk accepts).
  - disk2 `CryptsetupTestPassphrase` => reject, via `test_passphrase_fail(disk2)`
    fed through `.with_output_stdin(..., MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
    ...)`.
  - disk2 `is_luks_ok(disk2)` + `luks_dump_text_ok(disk2)` so the Rejected arm's
    `probe_luks_header` (mount.rs:526) classifies the header as `Ok` and the
    realistic "intact header, slot diverged" scenario is modeled.
  - `.with_mappers_closed(&["braid-disk1", "braid-disk2"])` -- **load-bearing for
    assertion (2), not decoration.** `ensure_luks_open` (`cli/src/luks.rs:914`)
    calls `classify_mapper_ownership` first, which issues `CryptsetupStatus`
    (`luks.rs:843`) *before* `CryptsetupLuksOpen` (`luks.rs:929`). Without a
    `CryptsetupStatus` mock, a regression that enters the open loop would die at
    the missing status probe *before* any open request is logged, so assertion
    (2) would pass for the wrong reason. Seeding the mappers as closed makes
    `classify_mapper_ownership` return `Inactive` (`luks.rs:848-850`), so the open
    loop actually reaches `CryptsetupLuksOpen`. On the correct
    (verify-rejects-disk2) path the open loop is never entered, so these status
    mocks go unused -- harmless.
- **No `CryptsetupLuksOpen` mocks.** MockRunner logs every request *before*
  dispatch (`cli/src/cmd.rs:1578/1592`) and returns `CmdError::MissingMock` for
  unmocked requests. Combined with the mappers-closed seed above, a regression's
  `CryptsetupLuksOpen` is logged to `runner.requests()` (then errors as
  `MissingMock`), so assertion (2) observes it and fails -- for the right reason.

### Assertions

```rust
let failure = execute_unlock_and_mount(/* ... */, &test_passphrase())
    .expect_err("non-first disk verify rejection should fail before any open");

// (1) verification reached the NON-first disk
assert!(runner.requests().iter().any(|r| matches!(
    r,
    CmdRequest::CryptsetupTestPassphrase { device }
        if device == "/dev/disk/by-id/virtio-disk2"
)));

// (2) NO mapper open issued for ANY disk -- verify-all precedes open-any
assert!(!runner.requests().iter()
    .any(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { .. })));

// (3) zero opened mappers reported (no partial open to clean up)
assert!(failure.opened_mappers.is_empty());

// (4) sanity anchor: the failure names disk2, not disk1
let msg = failure.error.to_string();
assert!(msg.contains("disk2"));
assert!(!msg.contains("disk1"));
```

Assertions (1) and (2) are the load-bearing additions; (3) and (4) keep parity
with the sibling and confirm the failure is genuinely disk2's verify rejection.
Avoid asserting the exact wording (`"wrong passphrase (rejected by disk2)"`) --
that string is already pinned by
`unlock_passphrase_verify_fails_ok_header_preserves_wrong_passphrase`
(`mount.rs:3330`); a loose `contains("disk2")` keeps this test structure-
insensitive.

## Why this regression test is well-formed

- **Behavioral, structure-insensitive:** asserts observable command requests
  (`runner.requests()`) and the failure value, not internal helper names or call
  counts.
- **Actually distinguishes the regression:** under "verify disk1 then open all",
  disk2's `CryptsetupTestPassphrase` is absent *and* disk1's `CryptsetupLuksOpen`
  is present -- assertion (1) and (2) both fail. Under correct code both hold.
- **Reads like its neighbors:** same fixtures, same entry point, same preamble
  form as `wrong_passphrase_zero_open_cleanup_is_noop`.

## Files

- `cli/src/mount.rs` -- add the test (only file changed). No production code,
  no new fixtures, no VM-test changes (this is a pure control-flow invariant
  best pinned by a Rust unit test against MockRunner; a VM test cannot cheaply
  assert "no CryptsetupLuksOpen issued").

## Verification

1. `just test-rust` -- the new test (and the whole `braid-cli` suite) passes.
2. Confirm the test actually guards the invariant (red/green check): temporarily
   apply the *targeted* regression in `open_disks_with_credential` -- verify only
   `&targets[..1]` (i.e. `verify_credential_for_targets(runner, &targets[..1],
   ...)`) while leaving the open loop iterating the full `to_unlock` -- run
   `just test-rust`, confirm **only** the new test fails, then revert. (Do this
   locally; do not commit the mutation.) Use this narrow mutation rather than
   "skip the verify match" or "open before verify": the broad versions also break
   the existing first-disk tests (`wrong_passphrase_zero_open_cleanup_is_noop`,
   `passphrase_mismatch_names_failing_disk`), which would make "only the new test
   fails" an unreliable signal. Verifying only `targets[..1]` keeps disk1 verified
   (so those first-disk tests still pass) while dropping disk2's preflight, which
   is exactly the regression this test exists to catch.
3. No fixture refresh needed -- no parser-critical tool versions change.
