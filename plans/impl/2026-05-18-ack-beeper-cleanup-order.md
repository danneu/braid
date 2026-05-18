# Plan: stop the beeper before file removals in ack cleanup

## Context

`AckError::CleanupFailed` (introduced in `c89353c`) carries this user-facing
contract:

> Re-running `braid ack` after fixing the I/O issue is idempotent.

That contract is false on one specific branch of `cleanup_alert_files_and_beeper`
(`cli/src/ack.rs:176-188`). The function runs:

```
remove_smartd_alert_flag (conditional)
remove_alert_latch
remove_alert_latch_corrupt
stop_beeper
```

When the third `remove_*` fails -- which the existing test
`cmd_ack_returns_cleanup_failed_when_corrupt_latch_cleanup_errors_after_baseline_saved`
at `cli/src/ack.rs:656-693` reproduces, EISDIR/EPERM via a poison directory at
`alert_latch_corrupt()` -- `?` short-circuits and `stop_beeper()` never runs.
The on-disk state at that point is:

- `alert_latch_json` removed
- `smartd_alert` removed (when `remove_smartd` was set)
- `alert_latch_corrupt` still the poison directory
- `braid-alert.service` still beeping

After the operator fixes the I/O fault (e.g. `rm -rf` the directory) and reruns
`braid ack`:

- `load_alert_latch -> Ok(None)`, so `causes` is empty and `latch_corrupt`
  is `false`.
- `smartd_active` is `false` (already cleaned).
- Mounted: hits the no-op gate at `cli/src/ack.rs:67-70`, prints "no active
  alerts", returns `Ok` -- never calls cleanup, never stops the beeper.
- Offline: `has_alert` is `false` at `cli/src/ack.rs:114-117`, returns
  `Err(AckError::PoolNotMounted)` -- never calls cleanup, never stops the
  beeper.

Operational impact: `braid-alert.service` is the only ongoing audible-loop unit
(`modules/braid/monitor.nix:87-120`, `Type = simple` exponential-backoff
`braid-beep-probe` loop when `beep` is enabled). The user must `systemctl stop
braid-alert.service` by hand, which contradicts the user-guide promise at
`manual/commands/ack.md:5` and the `CleanupFailed` rustdoc.

Other partial-cleanup branches do not have this bug:
- Failure at step 1 (`remove_smartd_alert_flag`) or step 2
  (`remove_alert_latch`) leaves the latch on disk, so the retry falls
  through the no-op gate, runs the full ack path, and reaches cleanup again
  -- which silences the beeper.
- Step 3 is unique because it runs *after* the latch is gone, leaving no
  on-disk evidence for the retry to gate on.

## Change

Move `stop_beeper()` to the very top of `cleanup_alert_files_and_beeper`,
before the conditional `remove_smartd_alert_flag` call.

`stop_beeper` returns no value: it logs a stderr warning when `Command::output`
fails to spawn or when `systemctl` exits non-zero (pinned by
`format_systemctl_stop_failure_warns_on_nonzero_exit_with_stderr` and
`format_systemctl_stop_failure_silent_on_zero_exit` at `cli/src/ack.rs:1199`
and `:1229`). So calling it before the `remove_*` chain cannot introduce a
new error to the cleanup return type -- it's a pure side-effect hook from
cleanup's perspective. This is the relevant property: the reorder is safe
because cleanup's `Result<(), io::Error>` shape is preserved, not because
`systemctl stop` guarantees the service is silenced.

The fix is about reaching the hook at all, not about strengthening what the
hook accomplishes once it runs. After the reorder, the FIRST cleanup
invocation in any branch reaches `stop_beeper()`, so a subsequent `remove_*`
failure can no longer trap the user behind an unreachable beeper-stop call
on retry.

## Files to modify

- `cli/src/ack.rs` -- single Rust file. No other module references
  `cleanup_alert_files_and_beeper` or `stop_beeper` (confirmed by repo-wide
  grep). No production wiring or callers need changes.
- `manual/commands/ack.md` -- single doc edit: the "What happens under the
  hood" ordered list at `:31-40` currently lists alert-file removals before
  the beeper stop, which becomes inaccurate after the reorder.

## Specifics

### 1. `cli/src/ack.rs:176-188` -- reorder cleanup

Move `stop_beeper()` to fire before any `remove_*`:

```rust
fn cleanup_alert_files_and_beeper(
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
    remove_smartd: bool,
) -> Result<(), std::io::Error> {
    stop_beeper();
    if remove_smartd {
        alert::remove_smartd_alert_flag(paths)?;
    }
    alert::remove_alert_latch(paths)?;
    alert::remove_alert_latch_corrupt(paths)?;
    Ok(())
}
```

### 2. `cli/src/ack.rs:159-175` -- update the function rustdoc

The current rustdoc says:

> A real I/O error on any `remove_*` short-circuits via `?`: subsequent
> removals and the `stop_beeper` invocation are skipped, and the error is
> propagated.

That statement becomes false with the reorder. Rewrite the relevant paragraph
to state the new invariant -- e.g. that `stop_beeper` runs first so partial
file-removal failures cannot block the beeper-stop hook from being invoked,
and a subsequent `remove_*` failure short-circuits the remaining removals
while propagating the I/O error. Keep the existing paragraph that explains
`remove_smartd`'s snapshot-scoped condition unchanged.

Be explicit that `stop_beeper` is best-effort: it issues `systemctl stop
braid-alert.service`, logs a warning on spawn failure or non-zero exit, and
returns no error to cleanup. The reorder guarantees the hook is reached on
every cleanup call, not that the audible alert is silenced.

### 3. `cli/src/ack.rs:247-257` -- refine `CleanupFailed` rustdoc

The current variant doc says "Cleanup of latch + smartd-alert + corrupt-latch
and the beeper hook failed". After the reorder, the beeper hook itself does
not fail; only the file removals can. Tighten the wording to "Cleanup of
latch + smartd-alert + corrupt-latch files failed after the best-effort
beeper stop hook had already run". Keep the trailing "Re-running `braid ack`
after fixing the I/O issue is idempotent" sentence -- it becomes truthful
and is the contract that motivated this fix.

Do not write "had already silenced the alert service" -- that overstates
what the best-effort hook can guarantee (it logs warnings on `systemctl
stop` failure, which is a real path on unknown units per
`cli/src/ack.rs:1189-1218`).

### 4. `cli/src/ack.rs:655-693` -- pin beeper ordering on the mounted step-3 failure path

Switch
`cmd_ack_returns_cleanup_failed_when_corrupt_latch_cleanup_errors_after_baseline_saved`
from `cmd_ack` to `cmd_ack_impl` with a beeper counter (same pattern as
`cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path` at
`cli/src/ack.rs:314-348`):

```rust
let beeper_calls = std::cell::Cell::new(0u32);
let beeper = || beeper_calls.set(beeper_calls.get() + 1);
let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper)
    .expect_err("cleanup failure must propagate");
```

Keep the existing assertions on `AckError::CleanupFailed`, the user-visible
message, durable baseline, removed latch, and persistent poison directory.
Add:

```rust
assert_eq!(
    beeper_calls.get(),
    1,
    "stop_beeper must fire even when a later cleanup remove_* fails"
);
```

Update the test's `// Intent:` / `// Why it exists:` preamble to mention the
new pin: the beeper-stop hook must run before any cleanup file-removal
failure can short-circuit, so the retry's no-op gate isn't behind an
unreached stop hook.

### 5. `cli/src/ack.rs:912-949` -- pin beeper ordering on the offline step-3 failure path

Same change to `ack_offline_cleanup_failure_after_missing_acked_returns_cleanup_failed`:
switch from `cmd_ack` to `cmd_ack_impl`, add the beeper counter, assert
`beeper_calls.get() == 1`, and extend the preamble. The offline call site
of cleanup is a separate code path from the mounted one (the regression
guards at `cli/src/ack.rs:86-88` and `cli/src/ack.rs:152-154`), so pinning
both is required.

### 6. New mounted step-1 ordering test

Sections 4 and 5 only exercise failure at the third `remove_*`. An
implementation that placed `stop_beeper()` between `remove_smartd_alert_flag`
and `remove_alert_latch` would still satisfy both tests while violating the
"top of cleanup" invariant the plan establishes. Pin step-1 ordering with one
additional mounted test alongside the section-4 test:

Setup:
- Latch carries `SmartdAlert` (so `latch_had_smartd = true` and therefore
  `remove_smartd = true`, even though `smartd_alert_active` returns false
  for non-files per `cli/src/alert.rs:281-287` and the
  `smartd_alert_active_requires_regular_file` test at
  `cli/src/alert.rs:756-781`).
- `std::fs::create_dir(paths.smartd_alert()).unwrap()` -- so
  `remove_smartd_alert_flag` calls `remove_file` on a directory and fails
  with EISDIR/EPERM.
- Mounted pool with healthy device-stats runner
  (`ack_mounted_probe_runner_with_device_stats()`).

Assertions:
- `AckError::CleanupFailed(_)` returned.
- `paths.acked_stats_json().exists()` -- baseline persisted before cleanup.
- `paths.alert_latch_json().exists()` -- cleanup short-circuited on step 1
  so the latch remains on disk (this distinguishes the step-1 failure path
  from step-3 in section 4).
- `paths.smartd_alert().exists()` -- poison directory still there.
- `beeper_calls.get() == 1` -- stop_beeper fired *before* the step-1
  removal failure. This is the ordering pin.

Preamble: name the step-1 ordering invariant explicitly. The two existing
CleanupFailed tests exercise the third removal; this test exercises the
first removal so the union pins "stop_beeper runs before every
remove_*", not just "stop_beeper runs sometime in cleanup".

### 7. `manual/commands/ack.md:31-40` -- update "What happens under the hood"

The current ordered list says the beeper stop runs after the file removals.
After the reorder it runs first. Edit the list to reflect the new order and
to mention all three file removals (the corrupt-latch sidecar is currently
omitted). Roughly:

```
3. Stops `braid-alert.service` (the beeper), best-effort. This runs first so
   the stop attempt is reached before any later file-removal I/O error can
   short-circuit the rest of cleanup.
4. Removes the smartd alert flag (`smartd-alert`) if present.
5. Removes the alert latch file (`alert-latch.json`).
6. Removes the corrupt-latch sidecar (`alert-latch.json.corrupt`) if present.
```

Match the code order from section 1 exactly: stop, then smartd flag, then
latch, then corrupt-latch sidecar. The current manual lists latch before
smartd, which already diverged from the code's `remove_smartd_alert_flag ->
remove_alert_latch -> remove_alert_latch_corrupt` order at
`cli/src/ack.rs:181-185` -- this edit corrects both the stop-order and the
pre-existing smartd/latch swap.

Keep the rest of the page (line 5 summary, "When to use it", "Basic
example", offline behavior paragraph, "Flags", "Safety checks", "Related
commands") unchanged.

## Out of scope

- `manual/commands/ack.md:5` -- the one-line summary ("Acknowledges active
  alerts and silences the PC speaker beeper") becomes truthful with this fix
  and needs no edit. Only the ordered "under the hood" list needs the
  reorder.
- The mounted no-op gate at `cli/src/ack.rs:67-70` and the offline no-alert
  branch at `cli/src/ack.rs:114-117` are intentionally **not** changed.
  Adding a defensive `stop_beeper()` to those branches would be scattered
  belt-and-suspenders against a state that the cleanup reorder already
  prevents from existing. Fix the source, not the symptoms.
- No sibling instances elsewhere -- `stop_beeper` and
  `cleanup_alert_files_and_beeper` are unique to `cli/src/ack.rs`. No
  cross-file refactor is justified.

## Verification

1. `just test-rust` -- runs the Rust unit tests.
   - The two modified tests (section 4: mounted step-3 CleanupFailed,
     section 5: offline step-3 CleanupFailed) must pass with their new
     `beeper_calls == 1` assertions.
   - The new step-1 test (section 6) must pass with
     `AckError::CleanupFailed` AND `beeper_calls == 1` AND latch still on
     disk.
   - Existing happy-path beeper-counter tests
     (`cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path`,
     `cmd_ack_with_mounted_pool_and_smartd_flag_no_latch_runs_full_ack_path`,
     `cmd_ack_with_mounted_pool_and_computation_error_only_latch_runs_full_ack_path`,
     `ack_offline_with_missing_device_cause_marks_missing_acked`,
     `ack_offline_corrupt_latch_still_clears_files`) keep expecting
     `beeper_calls == 1` -- the reorder doesn't change the call count.
   - NotBtrfs test (`cmd_ack_impl_with_foreign_fstype_does_not_invoke_beeper`)
     keeps expecting `beeper_calls == 0` -- cleanup is not reached on that
     path.

2. `just test-vm` -- VM tests should pass unchanged. None of them assert
   the exact ordering inside cleanup, only end-to-end behavior, which is
   unaffected when no I/O failure occurs.

3. Manual reasoning sanity check (no command -- just re-trace by reading
   the reordered function):
   - First attempt: poison at `alert_latch_corrupt()`. Cleanup calls
     `stop_beeper()` -> best-effort warning or success. Then
     `remove_alert_latch_corrupt` fails -> `CleanupFailed`. State: latch
     removed, sidecar still poison, beeper-stop hook **has been invoked**.
   - Operator removes the poison directory and reruns: mounted no-op gate
     fires, prints "no active alerts", returns `Ok`. No second
     `stop_beeper` call needed -- the first attempt already invoked it.
     Contract satisfied.
