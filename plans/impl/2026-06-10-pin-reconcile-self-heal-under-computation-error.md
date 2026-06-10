# Plan: pin reconcile self-heal persistence under a concurrent ComputationError fold

## Context

`cmd_monitor` (`cli/src/monitor.rs#cmd_monitor`) does two independent things in one
cycle:

1. **Step 6 (inside the `classified` closure):** `reconcile_acked_stats` self-heals
   `missing_acked` from `true` to `false` for a device that is present again, and if
   anything changed, saves `acked-stats.json`. This is the durable ack baseline.
2. **Steps 8-9 (after the closure returns):** load the alert latch, quarantine it if
   corrupt, and fold any failure plus the quarantine into *at most one*
   `ComputationError` cause.

The save in step 6 runs strictly before the latch path in step 8, so today a
successful self-heal save and a folded `ComputationError` coexist correctly. But no
test pins that **interaction**:

- `cli/src/monitor.rs#cmd_monitor_reconciles_acked_stats_across_pool_axes` covers self-heal
  persistence only on the clean `MonitorResult::Ok` path -- no error folded.
- `cli/src/monitor.rs#save_acked_stats_failure_latches_computation_error` covers the
  self-heal save *failing*, not succeeding alongside an error.
- `cli/src/monitor.rs#cmd_monitor_corrupt_alert_latch_latches_computation_error` folds a
  `ComputationError` from a corrupt latch but seeds no acked-stats, so reconcile never
  mutates or saves.

A refactor that moved or gated the reconcile save so it gets skipped whenever the same
cycle also raises a `ComputationError` (e.g. "we're returning an alert anyway, don't
bother persisting") would compile and pass every existing monitor test while silently
dropping the self-heal -- leaving the ack baseline stale so the next cycle re-fires or
mis-baselines the recovered device. This is the fail-closed safety surface for the NAS;
the file's convention is to pin exactly this class of structure-insensitive regression.

**Outcome:** one new behavioral unit test that fails if a future edit skips the
reconcile save when the cycle also folds a `ComputationError`. No production code
changes -- the current ordering is correct, and a behavioral test is the only thing
that can guard it against future restructuring.

## Change

Add one `#[test]` to the `tests` module in `cli/src/monitor.rs`, placed immediately after
`cli/src/monitor.rs#save_acked_stats_failure_latches_computation_error` so the three
reconcile-save tests read as a progression: clean-Ok self-heal -> save fails -> save
succeeds while the cycle also folds a `ComputationError`.

```rust
// Intent: cmd_monitor durably persists a reconcile self-heal (missing_acked
//   true -> false) in the same cycle that folds a ComputationError from a
//   corrupt alert latch.
// Why it exists: the reconcile save (step 6) runs inside the classified
//   closure, before the latch load/quarantine and ComputationError fold
//   (steps 8-9). cmd_monitor_reconciles_acked_stats_across_pool_axes covers
//   only the clean-Ok path and save_acked_stats_failure_latches_computation_error
//   covers only the save failing. A refactor that skipped or gated the reconcile
//   save whenever the cycle also raises a ComputationError would compile and pass
//   every other monitor test while silently dropping the self-heal -- leaving the
//   acked baseline stale so the next cycle re-fires or mis-baselines the device.
// Scenario: present devid 1 was previously acknowledged missing
//   (missing_acked=true) and is back online, while alert-latch.json is corrupt
//   this cycle. monitor must self-heal devid 1 to missing_acked=false and persist
//   it to acked-stats.json, AND return exactly one ComputationError for the
//   quarantined latch.
#[test]
fn reconcile_self_heal_persists_when_cycle_also_folds_computation_error() {
    let (_dir, paths) = isolated_paths();
    // Devid 1 is a present, recognized pool member (BTRFS_SHOW_2DISK) carrying a
    // stale missing ack; reconcile must self-heal it and save acked-stats.json.
    save_acked_stats(
        &alert::AckedStats(BTreeMap::from([("1".to_owned(), acked_disk(true, 1))])),
        &paths,
    )
    .unwrap();
    // Corrupt latch is the sole ComputationError source -- it is loaded AFTER the
    // reconcile save, so a healthy save must already be on disk by then.
    std::fs::write(paths.alert_latch_json(), b"not json").unwrap();

    let result = cmd_monitor(
        &MonitorTestRunner::with_stale_mapper_stats(),
        &monitor_fs_btrfs(),
        &monitor_mp(),
        &paths,
    );

    // Fold half: exactly one ComputationError, and the latch is its SOLE source.
    // The positive check alone is not enough -- folded_computation_error_detail
    // concatenates a failure detail and the latch detail in the (Some, Some) case,
    // so a co-folded "acked-stats unwritable" save failure would still contain the
    // latch substring. The negative check pins that no acked-stats failure was
    // folded, i.e. the reconcile save succeeded.
    let detail = assert_monitor_single_computation_error(&result);
    assert!(
        detail.contains("previous alert latch was unreadable -- quarantined"),
        "ComputationError must name the latch quarantine, got {detail}"
    );
    assert!(
        !detail.contains("acked-stats"),
        "no acked-stats failure may be folded in -- the reconcile save must have succeeded, got {detail}"
    );
    // Self-heal half: the reconcile write persisted despite the folded error.
    let reloaded = alert::load_acked_stats(&paths);
    assert_eq!(
        reloaded.0.get("1"),
        Some(&acked_disk(false, 1)),
        "self-heal must persist to acked-stats.json even when the cycle folds a ComputationError"
    );
}
```

## Design rationale

- **Runner: `MonitorTestRunner::with_stale_mapper_stats()`** (`cli/src/test_fixtures/monitor.rs#MonitorTestRunner::with_stale_mapper_stats`).
  It reports devids 1 and 2 as present + recognized with zero counters and **no missing
  devids** (the stale devid 99 row is unrecognized and ignored by `compute_alert_state`).
  So seeding a single `missing_acked=true` entry on present devid 1 yields exactly one
  self-heal and **no stray `MissingDevice` causes** -- the folded `ComputationError` is the
  only live cause, which is what `assert_monitor_single_computation_error` requires. This
  makes the new test the clean differential pair of
  `cmd_monitor_corrupt_alert_latch_latches_computation_error`: same runner and corrupt-latch
  setup, plus one seeded self-healable ack and the persistence assertion. (The alternative,
  `MonitorReconcileRunner`, reports null-underlying devid 2 and MISSING devid 3, forcing
  extra suppressor seeds that aren't load-bearing for this test's claim.)
- **Every seeded element is load-bearing:** the one ack entry is the self-heal subject; the
  corrupt latch is the fold source. Nothing incidental.
- **The detail assertions are a positive/negative pair, by design:** `cmd_monitor` folds a
  failure detail and the latch-quarantine detail into one `ComputationError` string
  (`cli/src/monitor.rs#folded_computation_error_detail`), so the positive latch-substring
  check alone cannot prove the latch is the sole source. Pairing it with
  `!detail.contains("acked-stats")` pins that no acked-stats save (or load) failure was
  co-folded -- i.e. the reconcile save succeeded -- reinforcing the persisted-self-heal
  claim from the other direction.
- **No `#[cfg(unix)]` gate:** the test only writes/reads files in the `isolated_paths()`
  temp dir (no permission bits), so it runs cross-platform -- matching the sibling
  corrupt-latch test, unlike the permission-based `save_acked_stats_failure...` test.
- **No new imports:** `BTreeMap`, `MonitorTestRunner`, `assert_monitor_single_computation_error`,
  `monitor_fs_btrfs`, `monitor_mp`, `isolated_paths`, `save_acked_stats`, `alert`, and the
  local `acked_disk` helper are all already in scope in the `tests` module
  (`cli/src/monitor.rs#tests`).

## Reused helpers (no new code beyond the test)

- `isolated_paths()`, `monitor_fs_btrfs()`, `monitor_mp()`,
  `assert_monitor_single_computation_error()`, `MonitorTestRunner::with_stale_mapper_stats()`
  -- `cli/src/test_fixtures/monitor.rs`.
- `acked_disk(missing_acked, read_io_errs)` -- local helper, `cli/src/monitor.rs#acked_disk`.
- `save_acked_stats` / `alert::load_acked_stats` -- `cli/src/alert.rs`.

## Out of scope

No change to `cmd_monitor` or any production code. The reconcile-save-then-latch-fold
ordering is already correct; the value here is the regression guard, which only a
structure-insensitive behavioral test can provide.

## Verification

1. Targeted run (fast iteration):
   `cargo test --lib --bin braid reconcile_self_heal_persists_when_cycle_also_folds_computation_error`
   -- expect it to pass.
2. Confirm it guards the regression: temporarily edit `cli/src/monitor.rs` so the step-6
   save is skipped when the cycle will fold an error (e.g. gate `save_acked_stats` on a
   condition that's false here), rerun the targeted test, confirm it **fails** on the
   reloaded-`missing_acked` assertion, then revert.
3. Full Rust suite: `just test-rust` -- expect green (the canonical recipe; the crate is
   `braid-cli` with binary `braid`).

No fixture refresh or parser-compat lanes apply -- this adds a test only, with no
`nixpkgs`/parser changes.
