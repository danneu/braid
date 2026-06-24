# Plan: pin EnospcRisk latch carry-forward across a monitor re-arm

## Context

ADR 014's [Severity tiers and the ENOSPC baseline](../../docs/design/decisions/014-alerts.md#severity-tiers-and-the-enospc-baseline)
states the ENOSPC re-arm invariant verbatim:

> This differs from the "latched until ack even if the condition disappears" rule
> only in the *post-ack* marker (re-arm on clear) ... The latch itself stays
> sticky-until-ack (`merge_into_latch` carries it forward), so the invariant holds.

So on a predicate-healthy **re-arm**, `cmd_monitor` must clear *only* the post-ack
snooze marker (`enospc-ack.json`) and keep any previously-latched `EnospcRisk`
cause in `alert-latch.json` until `braid ack`.

That invariant is currently **unpinned at the `cmd_monitor` / re-arm integration
point for the ENOSPC cause specifically**. The re-arm branch lives inside
`monitor.rs#evaluate_enospc_for_monitor`, which already holds `paths` and already
calls `remove_enospc_ack(paths)`. A *blanket* latch wipe there (e.g.
`alert::remove_alert_latch(paths)`) is already caught by an existing test (see "The
gap, precisely"). The genuinely uncaught regression is *cause-specific*: an edit
that -- on a "the risk cleared, so drop its latched cause" rationale --
retain-filters only `EnospcRisk` out of the loaded latch (keeping `MissingDevice`
and the other causes) would compile cleanly, leave every existing monitor and alert
test green, and silently violate latched-until-ack for `EnospcRisk`.

This is the testing gap from the finding. The production code is already correct;
the work is a single regression-guard unit test. Severity: Low (missing guard for
a documented invariant, not a present bug), but cheap and high-value.

## The gap, precisely

EnospcRisk carry-forward is covered today, but never at the point that matters --
a seeded `EnospcRisk` latch surviving the re-arm *integration* path:

- **Merge helper, in isolation:** `alert.rs#merge_carries_forward_latched_enospc_risk`
  proves `merge_into_latch(Some(latch_with_enospc), &[])` keeps the cause. No
  `cmd_monitor`, no re-arm branch, no `paths`.
- **Re-arm branch, but wrong cause:**
  `monitor.rs#healthy_cycle_carries_forward_existing_non_computation_latch` *does*
  execute the re-arm branch -- its `with_stale_mapper_stats()` serves the
  `usage_2disk_healthy()` payload, which re-arms (large positive predicate margin),
  over a no-missing `BTRFS_SHOW_2DISK` (so `missing_count == 0`, no early return).
  But its seeded latch is `MissingDevice{7}`, so it only proves a *non-ENOSPC* cause
  survives the branch: it catches a *blanket* latch wipe there, but not a filter that
  drops only `EnospcRisk`. It is the *only* existing test that reaches the re-arm
  branch with a latch on disk --
  `stats_failure_merges_existing_non_computation_latch_once` also seeds
  `MissingDevice`, but its injected stats-probe `Err` exits `cmd_monitor` at the
  step-2 stats probe, before the step-7b ENOSPC evaluation, so the re-arm branch
  never runs there.

So no single test pins that an `EnospcRisk` cause specifically survives the re-arm
integration path. The cause-specific retain-filter regression above leaves
`healthy_cycle_...` green (its `MissingDevice` survives the filter) and passes the
merge-helper test (which never runs `cmd_monitor`) -- this new test is the unique
guard.

The existing re-arm test `monitor.rs#cmd_monitor_rearms_on_predicate_health_then_refires`
seeds only the snooze **marker** (`seed_enospc_baseline`), never the **latch**, so
`existing_latch` is `None` at re-arm and it cannot observe carry-forward at all. Its
first assertion (`rearm == MonitorResult::Ok`) structurally *depends* on there being
no latch -- seed an `EnospcRisk` latch and re-arm correctly returns `Alert`, not
`Ok`. So the new coverage **cannot** be folded into it; it must be a sibling test.

Confirmed by exploration: no existing test in `cli/src/` both seeds an `EnospcRisk`
cause into `alert-latch.json` *and* runs `cmd_monitor` through a re-arm cycle.

## The fix

Add one Rust unit test to `cli/src/monitor.rs` `mod tests`, in the "ENOSPC-risk
monitor integration" section, immediately after
`monitor.rs#cmd_monitor_rearms_on_predicate_health_then_refires`.

It seeds **both** pieces of post-ack state that coexist in the real
risk -> ack(snooze) -> snooze-elapses-and-re-latches -> recover sequence:

- the **alert latch** with an `EnospcRisk` cause (the sticky-until-ack cause), via
  `alert::save_alert_latch`;
- the **snooze marker** (`enospc-ack.json`), via the existing `seed_enospc_baseline`,
  so the "marker removed" assertion is non-vacuous.

Then it runs a predicate-healthy re-arm cycle (`usage_4disk_one_low()`, whose RAID1
F2 predicate margin is large-positive -> re-arm) and asserts the three guarantees:
the marker is gone, the returned `Alert` still carries the exact seeded `EnospcRisk`,
and the latch round-trips to disk.

A distinctive sentinel margin (`-42`) makes the exact-equality assertion also catch a
hypothetical "re-arm fires a *fresh* EnospcRisk" regression -- a recomputed margin
would not equal `-42`.

```rust
// Intent: on a predicate-healthy re-arm, cmd_monitor clears ONLY the post-ack
//   snooze marker; a previously-latched EnospcRisk cause stays latched
//   (sticky-until-ack) and the latch round-trips.
// Why it exists: ADR 014 says re-arm differs from sticky-latch only in the
//   post-ack marker -- the latch stays sticky via merge_into_latch. Integration
//   coverage pins this only indirectly:
//   healthy_cycle_carries_forward_existing_non_computation_latch drives the same
//   re-arm branch but proves a MissingDevice latch survives it, and
//   merge_carries_forward_latched_enospc_risk proves the merge helper carries
//   EnospcRisk forward in isolation (no cmd_monitor). Neither pins that EnospcRisk
//   specifically survives the re-arm integration path. A re-arm edit that drops
//   only the latched EnospcRisk cause -- retain-filtering it out of the loaded
//   latch on a "risk cleared, so clear its latched cause" rationale -- would leave
//   both those tests green while violating latched-until-ack for the ENOSPC cause.
// Scenario: a prior cycle latched EnospcRisk and the operator snoozed it (marker
//   on disk); the pool's predicate margin then recovers to healthy.
#[test]
fn cmd_monitor_rearm_carries_forward_latched_enospc_risk() {
    let (_dir, paths) = isolated_paths();

    // Post-ack state that coexists after a re-fire: a latched EnospcRisk cause
    // plus the snooze marker the operator's ack wrote.
    let latched = AlertCause::EnospcRisk {
        margin: -42,
        count_below: 1,
        device_count: 2,
    };
    alert::save_alert_latch(
        &alert::AlertState {
            causes: vec![latched.clone()],
        },
        &paths,
    )
    .unwrap();
    seed_enospc_baseline(&paths, matching_pool_key(), open_snooze_deadline());

    // Predicate margin recovers -> re-arm.
    let runner = MonitorTestRunner::with_usage_payload(usage_4disk_one_low());
    let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

    // Re-arm clears ONLY the post-ack marker...
    assert!(
        !paths.enospc_ack_json().exists(),
        "re-arm must remove the snooze marker"
    );
    // ...the latched EnospcRisk cause itself stays sticky-until-ack, unchanged.
    let state = alert_state(&result);
    assert_eq!(
        state.causes,
        vec![latched],
        "latched EnospcRisk must carry forward across re-arm (sticky-until-ack)"
    );
    // ...and the carried-forward latch round-trips to disk.
    let saved = alert::load_alert_latch(&paths).unwrap().unwrap();
    assert_eq!(
        &saved, state,
        "latch must round-trip the carried-forward EnospcRisk"
    );
}
```

### Reused helpers (all already in scope; no new imports)

- `test_fixtures/doctor.rs#isolated_paths` -> `(TempDir, StatePaths)`
- `alert.rs#save_alert_latch` / `alert.rs#load_alert_latch`; existing monitor tests
  already call them as `alert::...` (see
  `monitor.rs#healthy_cycle_carries_forward_existing_non_computation_latch`)
- `monitor.rs` `mod tests` helpers `seed_enospc_baseline`, `matching_pool_key`,
  `open_snooze_deadline`, `alert_state`
- `test_fixtures/monitor.rs#with_usage_payload`, `#usage_4disk_one_low`,
  `#monitor_fs_btrfs`, `#monitor_mp` (already imported in `monitor.rs mod tests`)
- `AlertCause::EnospcRisk`, `alert::AlertState` -- in scope via `use super::*`

### Why a dedicated test, not a refactor/parametrization

The module pins latch carry-forward per-cause at the integration level because a
cause-specific drop is invisible to any test seeding a *different* cause. The re-arm
branch is reachable with any latched cause already on disk -- it already carries a
`MissingDevice` latch through it in `healthy_cycle_...` -- but nothing seeds an
`EnospcRisk` latch through it, so a filter that drops only `EnospcRisk` slips past
every existing test. A focused sibling test that seeds `EnospcRisk` and asserts it
survives the branch -- matching the established per-scenario style -- is the correct
shape. A parametrized/shared rewrite would dilute that targeted coverage. No
production code changes.

## Verification

1. **Run the suite:** `just test-rust` (runs `cargo test --lib ...`, which includes
   `cli/src/monitor.rs mod tests`). Or target it directly:
   `cargo test --manifest-path cli/Cargo.toml --lib cmd_monitor_rearm_carries_forward_latched_enospc_risk`.
   Expect: passes against current (correct) code.
2. **Prove it is a real guard (TDD "fails for the right reason"):** temporarily
   inject the *cause-specific* drop into the re-arm branch of
   `monitor.rs#evaluate_enospc_for_monitor`, after the `remove_enospc_ack(paths)`
   call and before `return None`:

   ```rust
   if let Ok(Some(mut latch)) = alert::load_alert_latch(paths) {
       latch.causes.retain(|c| !matches!(c, AlertCause::EnospcRisk { .. }));
       let _ = alert::save_alert_latch(&latch, paths);
   }
   ```

   Re-run the **full** suite. The new test must now **fail at the
   `alert_state(&result)` extraction**: with the latched `EnospcRisk` filtered out,
   the merged state is empty, `cmd_monitor` returns `MonitorResult::Ok`, and
   `alert_state` panics ("expected `MonitorResult::Alert`"). The marker assertion
   preceding it still passes, so the failure is specific to the dropped latch. Every
   other test -- including `healthy_cycle_carries_forward_existing_non_computation_latch`,
   which drives the *same* branch with a `MissingDevice` latch -- must stay **green**,
   confirming this new test is the unique guard for the ENOSPC-specific drop. (Do not
   substitute a blanket `alert::remove_alert_latch(paths)`: it would fail
   `healthy_cycle_...` too, so it proves nothing this test uniquely catches.) Revert;
   confirm green again.
3. No fixture/parser impact and no `flake.lock` change, so the parser-fixture refresh
   flow does not apply.
