# Fix: remove redundant probe_missing_devids in plan_remove_missing

## Context

`plan_remove_missing` (`cli/src/remove_missing.rs:303`) calls
`preflight::probe_missing_devids` to validate `--missing-id`, immediately
after `probe_pool` (line 251) has already populated `pool.missing_devids`
from `btrfs filesystem show`.

Two problems:

1. `PoolState::missing_devids` is documented in `cli/src/types.rs:63-67`
   as authoritative to btrfs ("from btrfs filesystem show MISSING
   sentinels"). `probe_missing_devids` instead calls `btrfs device usage
   --raw` and infers missing via `device_size == 0`. The two sources
   normally agree, but if parser behavior or btrfs output drifts they
   can diverge and produce contradictory validation errors inside one
   command.
2. It adds an extra `btrfs device usage --raw` invocation on the
   critical path for every `remove-missing`.

Fix: read from the authoritative source that's already in hand.

## Change

File: `cli/src/remove_missing.rs`

Delete the `probe_missing_devids` call + its error bail (lines 303-311)
and rewrite the subsequent membership check (line 312) to read from
`pool.missing_devids` directly:

```rust
if !pool.missing_devids.contains(&params.missing_id) {
    return RemoveMissingPlanReport {
        notes: std::mem::take(&mut notes),
        result: Err(RemoveMissingError::Validation(format!(
            "devid {} is not a device in this pool. \
             Use 'braid status' to see device IDs.",
            params.missing_id
        ))),
    };
}
```

The user-facing validation message is preserved verbatim.

Intentional behavior change: `btrfs device usage --raw` command/parse
failures are no longer consulted for `--missing-id` validation.
`btrfs filesystem show` failures still abort through `probe_pool` as
`RemoveMissingError::Probe` (that is the authoritative source for
`pool.missing_devids`). If the relocation-space preflight later calls
`btrfs device usage --raw` and it fails, that remains a soft warning
via `check_relocation_space`.

Also update the doc comment on `plan_remove_missing` (line 220) to drop
the `probe_missing_devids` reference from the list of responsibilities.

## Tests to update

All are in `cli/src/remove_missing.rs`.

### 1. `enospc_check_skipped_for_single_survivor` (lines 686-744)

The test's central assertion is now wrong in two ways:

- Change `usage_calls == 1` to `usage_calls == 0` (line 733).
- Update the comment on line 724 and the assertion message on lines
  733-735 to reflect that **no** `BtrfsDeviceUsageRaw` calls are
  expected on the single-survivor path (both the removed
  `probe_missing_devids` and the skipped ENOSPC `check_relocation_space`).
- Rename the test (e.g. `no_usage_probe_for_single_survivor`) since
  "enospc_check_skipped" is now only half the story. Optional but
  recommended.

Tighten the mock: remove the `BtrfsDeviceUsageRaw` arm from
`RecordingRunner::run` (lines 668-672). With the arm gone, any
regression that reintroduces a `BtrfsDeviceUsageRaw` call on the
single-survivor path will fail loudly with `CmdError::MissingMock`,
behavior-locking the zero-call invariant.

### 2. `ThreeDeviceSoftWarnRunner` (lines 1450-1536)

Only one `BtrfsDeviceUsageRaw` call remains in this path (from
`check_relocation_space`), so the call-count branching collapses:

- Delete the `usage_calls: Arc<Mutex<u32>>` field, its constructor init,
  and the lock/branch logic (lines 1494-1524).
- The `BtrfsDeviceUsageRaw` arm now unconditionally returns the
  failure dictated by `failure_mode` (the branch that used to run on
  the second call).
- Update the doc comment on lines 1434-1439 to describe a single-call
  runner.

The two tests that use this runner
(`plan_remove_missing_surfaces_soft_warn_on_command_error`,
`plan_remove_missing_surfaces_soft_warn_on_parse_error`) do not need
logic changes -- they assert on the resulting `PreviewNote::Warn` body,
which is untouched.

### 3. New focused test: wrong `--missing-id` still rejected

Current coverage mostly pins "redundant call is gone"; it does not
directly pin "the target-validation contract still holds against
`pool.missing_devids`." Add a focused `plan_remove_missing` unit test
in `cli/src/remove_missing.rs`:

- `btrfs filesystem show` fixture reports devid 3 as `MISSING` (reuse
  the 3-device `btrfs fi show` snippet already used elsewhere in the
  file).
- Invoke `plan_remove_missing` with `missing_id: 99`.
- Omit any `BtrfsDeviceUsageRaw` mock -- if the planner touches
  `btrfs device usage --raw` en route to this validation, the runner
  must fail with `MissingMock`, behavior-locking "no `device usage`
  probe before `--missing-id` rejection."
- Assert `report.result` is `Err(RemoveMissingError::Validation(msg))`
  with `msg == "devid 99 is not a device in this pool. Use 'braid
  status' to see device IDs."` (exact string, not substring).

Revert check: reintroducing `probe_missing_devids` must make this test
fail hard (`MissingMock`) rather than asserting a different path.

### 4. `preflight::probe_missing_devids` tests

`cli/src/preflight.rs:739-802` contains
`probe_missing_devids_returns_missing` and
`probe_missing_devids_returns_empty_when_healthy`. Keep as-is.
`probe_missing_devids` still lives and is still called by `replace.rs`
and `doctor.rs`, so its direct unit tests remain load-bearing.

### 5. VM test: `braid-remove-missing-softwarn`

Files: `tests/cli/braid-remove-missing-softwarn.py` and
`tests/cli/braid-remove-missing-softwarn.nix`.

The current `btrfs` wrapper (py lines 94-107) passes through the first
`btrfs device usage --raw` call and fails subsequent calls, on the
assumption that the first call is `probe_missing_devids` (validating
`--missing-id`) and the second is `check_relocation_space`. After the
fix, `probe_missing_devids` is gone and the relocation preflight is
the first (and only) `device usage --raw` call, so the `n >= 2`
threshold never trips and the expected `[warn]` line never appears --
the test will fail.

Update:

- `braid-remove-missing-softwarn.py`: change the wrapper to fail the
  first (and every subsequent) `btrfs device usage --raw` call.
  Concretely, replace the `if [ "$n" -ge 2 ]; then ... fi` gate with
  an unconditional `echo ... >&2; exit 1` inside the `device usage
  --raw` arm. The `COUNTER` file can stay or be removed; it is no
  longer load-bearing.
- Update the preamble comment (py lines 17-21) and the `.nix` file's
  header comment (`.nix` lines 17-21) to describe "fail the single
  `btrfs device usage --raw` call issued by `check_relocation_space`"
  -- drop the `probe_missing_devids` reference and the "first / second
  call" framing.
- Verify with `just test-vm braid-remove-missing-softwarn`.

No other VM test references `probe_missing_devids`'s call ordering
(grepping `tests/` confirms this one is the only PATH-shim that
depends on the two-call pattern), but run the full `just test-vm`
suite as part of verification.

## Out of scope

Do **not** delete `preflight::probe_missing_devids`. Remaining callers:

- `cli/src/replace.rs:767` -- dead/missing source resolution path in
  `braid replace`.
- `cli/src/doctor.rs:524` -- `check_pool_missing_devices` diagnostic.

Migrating those callers to `PoolState::missing_devids` is a reasonable
simplicity follow-up but is deliberately a separate change.

## Verification

1. `cargo test -p braid-cli --lib remove_missing` -- exercises the
   updated tests above, including the zero-call assertion, the new
   wrong-`--missing-id` focused test, and the soft-warn runner.
2. `cargo test -p braid-cli --lib preflight::tests::probe_missing_devids`
   -- confirms `probe_missing_devids` itself still works (for
   `replace` / `doctor`).
3. `just test-vm braid-remove-missing-softwarn` -- confirms the
   updated PATH-shim wrapper still surfaces the `[warn]` line on
   stdout under `--dry-run` and on stderr under real-run.
4. `just test-vm` (full VM suite) -- catches any other end-to-end
   drift in `remove-missing` wiring.

Behavioral regression targets (reverting the fix must break these):

- The single-survivor unit test's `usage_calls == 0` assertion fails,
  and with the mock arm removed the call itself fails `MissingMock`.
- The new wrong-`--missing-id` unit test fails `MissingMock` (because
  a restored `probe_missing_devids` would touch
  `BtrfsDeviceUsageRaw` before the `contains` check).
- `braid-remove-missing-softwarn` still passes after the wrapper
  change (meaning the one remaining `device usage --raw` call is the
  relocation preflight, not a resurrected `probe_missing_devids`).
