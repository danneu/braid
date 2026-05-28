# Plan: pin missing-context replace hint at the `cmd_remove_missing` layer

## Context

The `verify-issue` investigation of the "1 survivor + 2 missing" finding
confirmed a real but narrow test gap. The planner at
`cli/src/remove_missing.rs:400-457` intentionally restricts its early
reject to the `total_devices == 2 && devices.len() == 1 && missing_count == 1`
case; the comment at lines 405-411 explicitly delegates multi-missing
reasoning to the kernel plus `device_remove_error`
(`cli/src/pool.rs:314-353`), which appends a Missing-context replace
hint when stderr matches `"unable to go below"`.

That contract has two existing pins:

- `pool_remove_device_using_failure_emits_missing_replace_hint`
  (`cli/src/pool.rs:1492-1536`) -- pins the wrapper, but only the wrapper.
- `journal_survives_device_remove_failure`
  (`cli/src/remove_missing.rs:1709-1792`) -- pins journal/pool.json
  preservation on a device-remove failure, but uses `"No space left on
  device"` stderr, which never reaches the `"unable to go below"` branch
  in `device_remove_error`.

No test exercises the wiring from `cmd_remove_missing` through
`pool_remove_device_using` to the missing-context replace hint. A
regression that broke that wiring -- e.g. swapping
`RemoveContext::Missing` for `::Live` at the call site, replacing the
hint with a generic message, or swallowing the `PoolError` -- would
ship silently.

The finding's prescribed fix is heavier than necessary: it asks for a
new `3-total / 1-present / 2-missing` fixture so the test can also
pin that the planner *doesn't* reject that topology. The device-remove
failure is mocked, so the wiring contract is topology-independent; and
asserting that the planner doesn't reject 1+2 would freeze the
"intentionally out of scope" design choice (line 408) as a hard
behavioral contract, which is a non-trivial trade-off the finding does
not surface. The minimal, focused fix is one new sibling test reusing
the existing 2+1 fixture.

## Change

Add one Rust unit test next to
`journal_survives_device_remove_failure` in
`cli/src/remove_missing.rs` (`tests` mod, around line 1792).

The new test mirrors the existing test's structure exactly, with two
differences:

1. The mocked `BtrfsDeviceRemove` stderr is the kernel's RAID1C3
   min-devices rejection (`"ERROR: error removing device '3': unable to
   go below three devices on raid1c3"`) instead of ENOSPC. The plain
   `"unable to go below two devices on raid1"` variant is *not* the
   realistic kernel failure mode for the
   `three_disk_one_missing` topology (3-total / 2-present / 1-missing):
   after removing the missing devid the pool would land at 2 devices,
   which meets the RAID1 minimum, so the kernel would not reject. The
   CLI-reachable fallback that the `"intentionally out of scope"`
   comment defers to is the broader `"unable to go below"` family --
   most realistically a stray RAID1C3 chunk left behind. This is the
   same scenario already pinned at the wrapper layer by
   `device_remove_result_missing_raid1c3_min_includes_replace_hint`
   (`cli/src/pool.rs:1356`) and
   `pool_remove_device_using_failure_emits_missing_replace_hint`
   (`cli/src/pool.rs:1492`); the new test lifts that same stderr one
   layer up to `cmd_remove_missing`. Either way the
   `"unable to go below"` substring is what reaches the
   missing-context replace-hint branch in `device_remove_error`
   (`cli/src/pool.rs:320-340`).
2. Additional assertions verify the surfaced error message contains
   the Missing-context hint and does not leak the Live-context hint --
   matching the assertion shape from
   `pool_remove_device_using_failure_emits_missing_replace_hint`
   (`cli/src/pool.rs:1514-1535`).

## Files modified

- `cli/src/remove_missing.rs` -- add one `#[test]` plus three-section
  preamble. No other production or test files change. No new fixtures
  needed; `RemoveMissingPool::three_disk_one_missing()` and
  `PoolFixture::three_disk_devids_pinned()` from
  `cli/src/test_fixtures/remove_missing.rs` are reused as-is.

## Test preamble (Intent / Why / Scenario)

Per the project convention in
[`docs/dev/testing.md`](../docs/dev/testing.md):

```rust
// Intent: cmd_remove_missing surfaces device_remove_error's Missing-context
//   replace hint when btrfs rejects with "unable to go below" min-devices,
//   alongside journal preservation.
// Why it exists: the planner intentionally leaves multi-missing topologies
//   to the kernel + device_remove_error (see comment at remove_missing.rs
//   lines describing the narrow 2-disk reject). pool_remove_device_using's
//   own test pins the wrapper, but only this command-level test catches a
//   regression in the wiring -- e.g. swapping RemoveContext::Missing for
//   ::Live, swallowing the PoolError, or replacing the hint with a generic
//   message.
// Scenario: 3-disk NAS, devid 3 dies. Operator runs `braid remove-missing
//   --missing-id 3`. A stray RAID1C3 chunk left over from an earlier
//   conversion still requires three devices, so btrfs refuses the
//   device-remove call with the RAID1C3 min-devices rejection (the same
//   stderr shape already pinned at the pool-helper layer by
//   device_remove_result_missing_raid1c3_min_includes_replace_hint).
//   The journal must survive AND the operator must see the replace
//   command + recover hint, not raw kernel stderr.
```

## Assertions

The new test asserts:

- `result` is `Err`.
- Error string contains `"btrfs device remove failed (exit 1)"`.
- Error string contains `"hint:"`.
- Error string contains
  `"braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>"`.
- Error string contains `"braid recover"`.
- Error string does NOT contain `"braid replace --missing-id"`.
- Error string does NOT contain `"dconvert=raid1"` (the Live-context hint).
- Journal survives (same shape as the ENOSPC sibling: three pre-members,
  two target-members, target disk3 still in pool.json).
- `BtrfsDeviceRemove` was issued; `BtrfsBalanceRaid1Soft` was not.
- `f.inhibitor.acquire_count() == 1`.

## Reused utilities

- Fixture: `PoolFixture::three_disk_devids_pinned()`
  (`cli/src/test_fixtures/remove_missing.rs:189-212`).
- Topology installer: `RemoveMissingPool::three_disk_one_missing()`
  (`cli/src/test_fixtures/remove_missing.rs:88-95`).
- Params builder: `f.remove_missing_params()`
  (`cli/src/test_fixtures/remove_missing.rs:246-258`).
- Journal loader: `journal::load_journal(&f.paths)`.
- Assertion model: copy from
  `journal_survives_device_remove_failure`
  (`cli/src/remove_missing.rs:1709-1792`) and graft the hint assertions
  from `pool_remove_device_using_failure_emits_missing_replace_hint`
  (`cli/src/pool.rs:1514-1535`).

## Verification

- `just test-rust` -- the new `#[test]` runs in `cli` crate unit tests
  and must pass.
- Sanity check the failure mode by temporarily editing
  `pool_remove_device_using` at `cli/src/pool.rs:538` to call
  `device_remove_result(RemoveContext::Live, mount_point, &result)`
  instead of `RemoveContext::Missing` (do not commit); the new test
  must fail with a missing `braid replace --old ...` substring (and
  may also flag the unexpected `dconvert=raid1` Live-context wording),
  while `pool_remove_device_using_failure_emits_missing_replace_hint`
  at `cli/src/pool.rs:1492` should fail for the same reason. Revert
  before committing.
- No VM-test, fixture-capture, or docs changes required. The
  `cmd_remove_missing` -> `pool_remove_device_using` ->
  `device_remove_error` path is fully exercised by unit tests once the
  new sibling lands.
