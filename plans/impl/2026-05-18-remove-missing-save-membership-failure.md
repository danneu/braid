# Plan: Add `remove-missing` post-mutation membership persist failure coverage

## Summary

Add a Rust unit test in `cli/src/remove_missing.rs` that exercises the
failure phase the VM test cannot reach: `btrfs device remove` succeeds,
then `membership::save_membership` fails while persisting `pool.json`.

This is a coverage-only change. Do not change production behavior, public
APIs, command output, or the VM test.

## Key Changes

- Add a new unit test near `journal_survives_device_remove_failure` and
  `journal_survives_soft_balance_failure`, named
  `journal_survives_save_membership_failure_after_device_remove`.
- Use the existing test seams:
  - `PoolFixture::three_disk_devids_pinned()`
  - `RemoveMissingPool::three_disk_one_missing().install(MockRunner::default())`
  - `MockRunner::with_handler` overriding `CmdRequest::BtrfsDeviceRemove`
- In the `BtrfsDeviceRemove` override:
  - Set the returned `remove_done` flag to `true` with `Ordering::SeqCst`.
  - Remove the existing temp `pool.json`.
  - Create a directory at the same `pool.json` path.
  - Return `Ok(mock_ok("btrfs device remove", ""))`.
- This forces the later `membership::save_membership` atomic rename into
  `pool.json` to fail after the mocked btrfs mutation has already succeeded.
- Add the required Rust test preamble using `// Intent`, `// Why it exists`,
  and `// Scenario`.

## Expected Assertions

The new test should assert:

- `cmd_remove_missing(...)` returns an error.
- The error string contains `failed to persist pool membership`.
- The error string contains `pool.json`.
- `runner.requests()` includes `CmdRequest::BtrfsDeviceRemove`, proving the
  test reached the post-mutation phase.
- `runner.requests()` does not include `CmdRequest::BtrfsBalanceRaid1Soft`,
  proving post-remove maintenance did not run after the persist failure.
- `journal::load_journal(&f.paths)` returns `Some(...)`, proving
  `pending-op.json` survives for recovery.
- The loaded journal remains
  `OpKind::RemoveMissing { phase: RemoveMissingPhase::PoolMutation, .. }`,
  because `rewrite_journal` must not run until after `pool.json` is
  successfully persisted.

## Interfaces

No public API, CLI, schema, fixture-constructor, or VM-test changes.

Use only a test-local failure injection through the existing
`MockRunner::with_handler` seam.

## Test Plan

- Run:
  ```sh
  just test-rust
  ```
- No VM test is required for this follow-up because the target failure phase
  is intentionally unit-level and cannot be reached by the read-only
  state-dir VM setup.

## Assumptions

- This follow-up is about closing the coverage gap only.
- `remove_missing` should continue returning its current `Validation`-shaped
  error for this save failure unless a separate plan changes post-commit
  error classification.
