# Plan: pin healthy-cycle latch carry-forward at `cmd_monitor` level

## Context

ADR 014 (`docs/design/decisions/014-alerts.md:52`, `:114`) makes
"alerts persist until `braid ack` -- even if the triggering condition
disappears" a first-class invariant. `cmd_monitor` enforces it by
unconditionally loading the existing latch and feeding it through
`merge_into_latch` at `cli/src/monitor.rs:135` and `:144` before saving
back. Today that wiring is pinned by two layers of tests:

- `cli/src/alert.rs:1314-1322` (`merge_no_new_causes_carries_forward_latched`)
  -- the helper in isolation.
- `cli/src/monitor.rs:642-681` (`stats_failure_merges_existing_non_computation_latch_once`)
  -- the helper called from `cmd_monitor`, but only on the failure-detail
  branch (stats command returns `Err`).

There is no `cmd_monitor`-level test for the **healthy** branch: probe
ok, stats ok, no smartd flag, with a pre-existing active latch. A
regression that gated `merge_into_latch` on `failure_detail.is_some()`,
passed `None` for `existing_latch` on the success branch, or skipped the
latch load entirely would compile clean and pass every existing unit
test (helper test still green because it tests the helper directly;
failure-path mirror still green because it never enters the success
branch) while silently violating latched-until-ack on a recovered pool.

The fix is one focused regression test. The verify-issue investigation
already confirmed every fixture needed is in place; no production code
change is warranted.

## Plan

### Single change: add a regression test

**File:** `cli/src/monitor.rs`, inside `mod tests` (the `#[cfg(test)]`
block starting at `:161`).

**Position:** Immediately after
`stats_failure_merges_existing_non_computation_latch_once` (currently
ending at `:681`), before `monitor_classifies_non_btrfs_mount_as_offline`
(`:700`). The two tests are mirrors across the
`failure_detail.is_some()` boundary; adjacency makes the symmetry
visible and a future drift in one half visible against the other.

**Test name:**
`healthy_cycle_carries_forward_existing_non_computation_latch`. Echoes
the failure-path sibling by swapping `stats_failure` for `healthy_cycle`.

**Imports:** None. `isolated_paths`, `MonitorTestRunner::with_stale_mapper_stats`,
`monitor_fs_btrfs`, `monitor_mp`, `alert_state`, `alert::AlertState`,
`alert::save_alert_latch`, `alert::load_alert_latch`, and
`AlertCause::MissingDevice` are all reachable through the existing
`use super::*;` and `use crate::test_fixtures::{...};` lines at
`cli/src/monitor.rs:163-169`. Same call shape already used at `:647`
and `:676`.

**Body** (matches the failure-path mirror's three-section `//`
preamble per the project's Test Conventions in `AGENTS.md`):

```rust
// Intent: A fully healthy cycle -- probe ok, stats ok, no smartd flag --
//   still loads, merges, and re-persists a pre-existing active latch, so
//   an alert survives until braid ack even after the triggering
//   condition resolves.
// Why it exists: ADR 014's sticky-latch invariant is pinned at
//   cmd_monitor level only via
//   stats_failure_merges_existing_non_computation_latch_once, which
//   exercises the failure-detail branch.
//   merge_no_new_causes_carries_forward_latched covers the helper in
//   isolation. A regression that gated merge_into_latch on
//   failure_detail.is_some(), passed None for existing_latch on the
//   success branch, or skipped the latch load entirely would compile
//   and pass every other monitor unit test while silently violating
//   latched-until-ack on a recovered pool.
// Scenario: a prior cycle latched MissingDevice { devid: 7 }, then the
//   next cycle finds the pool healthy -- btrfs reports the same two
//   members with zero counters, no smartd flag, no probe failure.
//   monitor must return MonitorResult::Alert carrying
//   MissingDevice { devid: 7 } and re-persist it so alert-latch.json
//   reloads to the returned state.
#[test]
fn healthy_cycle_carries_forward_existing_non_computation_latch() {
    let (_dir, paths) = isolated_paths();
    let existing = alert::AlertState {
        causes: vec![AlertCause::MissingDevice { devid: 7 }],
    };
    alert::save_alert_latch(&existing, &paths).unwrap();

    let result = cmd_monitor(
        &MonitorTestRunner::with_stale_mapper_stats(),
        &monitor_fs_btrfs(),
        &monitor_mp(),
        &paths,
    );

    let state = alert_state(&result);
    assert_eq!(
        state.causes,
        vec![AlertCause::MissingDevice { devid: 7 }],
        "healthy cycle must carry forward the latched cause unchanged"
    );

    let saved = alert::load_alert_latch(&paths).unwrap().unwrap();
    assert_eq!(
        &saved, state,
        "saved latch must match returned monitor alert"
    );
}
```

### Shape choices and why

- **Exact-vector equality on `state.causes`**, not the
  `.filter().count() == 1` form used by the failure-path mirror. The
  mirror has to coexist with the `ComputationError` injected by
  `folded_computation_error_detail`; the healthy branch has no such
  contamination, so equality catches both losses (latch dropped) **and**
  spurious additions (a stale row leaking a cause, smartd state
  misread, etc.). Stricter is better here.
- **`with_stale_mapper_stats()`** is the canonical healthy runner in
  this module (used by eight other healthy-path tests, e.g.
  `cli/src/monitor.rs:336`, `:484`, `:613`, `:701`, `:734`, `:835`).
  Re-using it keeps the new test indistinguishable from a clean
  two-disk pool except for the seeded latch, which is the variable
  under test. Its benign zero-counter devid-99 stale row also pins
  incidentally that membership reconciliation doesn't leak a cause
  into the latch.
- **Round-trip via `load_alert_latch`** mirrors the failure-path
  sibling's tail at `cli/src/monitor.rs:676-680` and pins the re-save
  step at `cli/src/monitor.rs:147-151`, not just the return value.
  Without it, a regression that returned the correct merged state but
  failed to call `save_alert_latch` on the success path would still
  pass the first assertion.

### Non-goals

- No production code changes. The wiring at
  `cli/src/monitor.rs:135-158` is correct today; this test pins it.
- No new fixtures, runners, or helpers.
- No expansion to other carry-forward scenarios (smartd-only,
  multi-cause, etc.). Those would be separate tests; not part of this
  finding's gap.

## Verification

1. **Functional**: `just test-rust`. New test passes alongside the
   existing suite. (Per `AGENTS.md`, the crate package is `braid-cli`,
   so `just test-rust` is preferred over `cargo test -p ...`.)
2. **Mutation check** (load-bearing-ness proof, ~30 seconds):
   the mutation must isolate the success branch so the existing
   failure-path sibling stays green -- `stats_failure_merges_existing_non_computation_latch_once`
   also seeds `MissingDevice { devid: 7 }` and asserts it survives
   (`cli/src/monitor.rs:644-662`), so an unconditional `None` at line
   144 would fail both tests and prove nothing about branch coverage.
   The branch signal `failure_detail` is moved into
   `folded_computation_error_detail(...)` at `cli/src/monitor.rs:139`
   (it is `Option<String>`, not `Copy`), so it cannot be read at line
   144 directly -- the recipe must stash a boolean first. Two
   temporary edits:

   1. Immediately before `cli/src/monitor.rs:139`, insert:
      `let success_branch = failure_detail.is_none();`
   2. Change `cli/src/monitor.rs:144` from
      `merge_into_latch(existing_latch.as_ref(), &live_causes)` to
      `merge_into_latch(if success_branch { None } else { existing_latch.as_ref() }, &live_causes)`.

   Rerun `just test-rust` and confirm **exactly**
   `healthy_cycle_carries_forward_existing_non_computation_latch`
   fails. The existing failure-path sibling and the alert.rs helper
   test should still pass -- that asymmetry is what proves the new
   test is the unique guard for the success branch. Revert both edits
   before committing.
3. **Optional broader sweep**: skip. The change is one test addition
   with no production impact; the full VM suite is not warranted (per
   `AGENTS.md` Test scope guidance).
