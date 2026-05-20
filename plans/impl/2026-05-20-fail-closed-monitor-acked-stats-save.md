# Fail closed on monitor acked-stats save failure

## Context

`cmd_monitor` runs the Layer-3 defense-in-depth reconcile of `acked-stats.json` (ADR 014:142). Every other I/O failure inside the monitor closure -- probe failure, btrfs stats run/parse failure, `load_acked_stats_fallible` read failure, alert-latch corruption -- folds into a single `ComputationError` cause, gets latched, and surfaces as `MonitorResult::Alert` so the systemd wrapper starts the beeper (ADR 014:74).

The one exception: when `reconcile_acked_stats` mutates `acked` in memory and the follow-up `save_acked_stats` write fails, `cli/src/monitor.rs:110-112` only emits `eprintln!("Warning: failed to update acked stats: {e}")` and continues. With no other live cause, `MonitorResult::Ok` is returned, exit 0, no beep. A persistently failing `/var/lib/braid` write (EROFS, ENOSPC, EACCES on the file or its parent) becomes silent except for one stderr line per cycle that journald captures and nobody hears.

This violates ADR 014:74's fail-closed contract for indeterminate persistence state. The in-memory reconcile is correct each pass, but the stale on-disk acked-stats means hygiene is genuinely indeterminate, and the operator gets no signal.

The fix routes the save failure through the existing closure-level `Err(detail)` -> `folded_computation_error_detail` -> `ComputationError` -> `merge_into_latch` -> `MonitorResult::Alert` pipeline, matching the other I/O failures on the same path.

Scope is intentionally tight: `save_alert_latch` warning-only behavior at `cli/src/monitor.rs:144-148` is *not* part of this fix. That save persists the latch itself; failing-closed-via-latch is circular, and `MonitorResult::Alert` already drives exit-1/beep through the wrapper regardless of whether the latch persisted. The only loss there is `braid status` visibility, not the fail-closed signal.

## Changes

### `cli/src/monitor.rs` (single closure edit)

Replace the warn-and-continue block at lines 108-112:

```rust
let ack_changed =
    alert::reconcile_acked_stats(&mut acked, &still_relevant_devids, &present_devids);
if ack_changed && let Err(e) = save_acked_stats(&acked, paths) {
    eprintln!("Warning: failed to update acked stats: {e}");
}
```

with closure-level error propagation that mirrors the existing read-failure form at line 91:

```rust
let ack_changed =
    alert::reconcile_acked_stats(&mut acked, &still_relevant_devids, &present_devids);
if ack_changed {
    save_acked_stats(&acked, paths)
        .map_err(|e| format!("acked-stats unwritable -- {e}"))?;
}
```

Detail wording mirrors `"acked-stats unreadable -- {e}"` for symmetry with the load-side failure. The closure's outer match at lines 122-129 already prints `error: {detail}` to stderr, so no separate eprintln is needed; the existing fold path handles latching.

No other lines in `monitor.rs` change. The closure's return type stays `Result<Option<Vec<AlertCause>>, String>`. `folded_computation_error_detail` already covers `(Some(failure), None)` -- the natural shape when a save failure fires alone.

### `cli/src/monitor.rs` tests (one new test)

Add `save_acked_stats_failure_latches_computation_error` alongside the existing reconcile tests (around `cli/src/monitor.rs:212`). Model the seed on `cmd_monitor_reconciles_acked_stats_across_pool_axes`: pre-write an orphan acked entry so `reconcile_acked_stats` mutates state and `ack_changed` is true; the existing `MonitorReconcileRunner` already supplies a pool topology (devid 1 present, devid 2 null-underlying, devid 3 MISSING) that prunes a devid-99 orphan.

Inject the save failure by making the state directory non-writable after the seed, following the existing chmod-based fail-injection precedent at `cli/src/alert.rs:957-979` (`quarantine_link_failure_surfaces_in_detail`):

1. Gate the test with `#[cfg(unix)]` and bring `std::os::unix::fs::PermissionsExt` into scope.
2. After `save_acked_stats` writes the seed entry, capture the original perms with `std::fs::metadata(...).unwrap().permissions()`, then `std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o500)).unwrap()`. `0o500` (r-x------) preserves owner read+execute -- so `load_acked_stats_fallible` still succeeds -- while blocking the directory entry creation that `atomic_write` needs for its `.tmp` file. Use `0o500` rather than `0o555` for consistency with the alert.rs precedent.
3. Install a small `RestorePerms { path, perms }` struct with a `Drop` impl that calls `let _ = std::fs::set_permissions(&self.path, self.perms.clone());` before `cmd_monitor` runs, mirroring `cli/src/alert.rs:962-970`. This guarantees the TempDir Drop can clean up even if a later assertion panics.
4. Behavioral injection only -- the test must not reference `atomic_write`'s temp-suffix naming or any other private state-io implementation detail. A future refactor of `state_io.rs` that keeps "write to a read-only dir fails" must keep this test passing.

Assertions reuse `assert_monitor_single_computation_error` (from `cli/src/test_fixtures/monitor.rs:266`): expect `MonitorResult::Alert` with exactly one `ComputationError`, and assert its detail contains both `"acked-stats"` and `"unwritable"` so a future detail-wording change is caught. Do *not* assert `paths.alert_latch_json().exists()`: the same read-only-dir fault that fails `save_acked_stats` also fails `save_alert_latch` at `cli/src/monitor.rs:144-148`, where it stays warning-only by design (see "Why this shape" below). The fail-closed signal is the `MonitorResult::Alert` return value, which `main.rs:861-863` turns into exit 1; latch persistence is a separate, weaker guarantee that this test deliberately doesn't pin.

Test preamble follows the `//` line-comment form documented in `docs/testing.md:11-22` and used by the existing line-commented tests at `cli/src/monitor.rs:289`, `451`, `486`, `524` and `cli/src/alert.rs:950-956`. The three sections are `// Intent: ...`, `// Why it exists: ...`, `// Scenario: ...`, with continuation lines also prefixed by `//`. Do not use the older `/* */` block form even though some sibling tests still do; new tests follow the documented convention.

### `docs/decisions/014-alerts.md` (one sentence)

Layer 3's paragraph at `docs/decisions/014-alerts.md:142` currently documents the read side only:

> The read itself uses `load_acked_stats_fallible` so a corrupt or unreadable `acked-stats.json` latches `ComputationError` instead of silently re-firing acked causes against an empty baseline, matching offline ack and `drop_ghost_acked_for_devids`.

Append a sentence covering the write side:

> A save failure during reconcile latches the same `ComputationError` so a persistent FS write fault (EROFS, ENOSPC, or EACCES on `acked-stats.json` or its parent) surfaces via exit-1 beep rather than accumulating only in journald.

No other ADR changes; the existing fail-closed wording at ADR 014:74 already covers the principle.

## Why this shape (and what was rejected)

- **Closure `?` propagation** beats "save into a sibling `Option<String>` like `latch_corrupt_detail`": the save call lives inside the closure where `?` already routes every other I/O failure, so the existing fold path handles it with zero new plumbing. The `latch_corrupt_detail` pattern exists because `load_alert_latch_or_quarantine` runs *outside* the closure -- a different structural slot.
- **Skipping `compute_alert_state` after save failure is acceptable.** The in-memory `acked` is correct, but if the FS write is broken, the next cycle re-loads, re-reconciles in memory, and re-fails the save. Underlying alerts (counter-based `BtrfsDeviceErrors`, `MissingDevice`) re-fire on the next pass once the FS issue clears. Losing this cycle's live causes in exchange for a single ComputationError that names the persistent write fault is the correct fail-closed trade.
- **Not bundling `save_alert_latch`.** That save's failure mode is logically distinct (the latch IS the persistence layer; circular fail-closed) and `MonitorResult::Alert` already triggers exit-1/beep regardless of whether persistence succeeded. Folding it into this fix would muddy scope.

## Critical files

- `cli/src/monitor.rs` -- the closure edit at 108-112; new test alongside 212.
- `cli/src/alert.rs:82-89` -- `save_acked_stats` / `save_acked_stats_at`; not modified, just the call site target.
- `cli/src/alert.rs:957-979` -- `quarantine_link_failure_surfaces_in_detail`; the precedent test the new test copies for chmod-based fail-injection and the `RestorePerms` Drop guard pattern.
- `cli/src/test_fixtures/monitor.rs` -- `MonitorReconcileRunner`, `assert_monitor_single_computation_error`, `isolated_paths`, `monitor_fs_btrfs`, `monitor_mp` are all already in place; no fixture changes needed.
- `docs/testing.md:11-22` -- documented `//` line-comment preamble form the new test must follow.
- `docs/decisions/014-alerts.md:142` -- ADR sentence append.

## Verification

1. `just test-rust` -- the new unit test must pass, all existing monitor tests must keep passing. The existing `cmd_monitor_reconciles_acked_stats_across_pool_axes` is the closest sibling and exercises the success path of the same `save_acked_stats` call.
2. `just test-vm braid-monitor` -- only if `just test-rust` reveals any regression in monitor-side wording the VM tests assert on. Expected to be a no-op.
3. Spot-check by reading the closure: the `Err(detail)` path now contains five mapped failures (probe, stats run, parse, load-fallible, **save**); the outer match at lines 122-129 already prints `error: {detail}` and folds the detail into a `ComputationError` via `folded_computation_error_detail`. No new code paths beyond the one `?`.
