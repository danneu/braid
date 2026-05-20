# Plan: stop unrecognized-devid stats rows from latching `BtrfsDeviceErrors`

## Context

`cmd_monitor` (`cli/src/monitor.rs:99-118`) runs two passes that disagree about which devids matter:

- `reconcile_acked_stats` (alert.rs:253-274) prunes acked entries whose devid is not in `still_relevant = present + null_underlying + missing` (the set built inline at monitor.rs:102-107).
- `compute_alert_state` (alert.rs:102-137) walks **every** row in `btrfs device stats` output and emits `BtrfsDeviceErrors { devid }` for any row with non-zero counters and no acked baseline.

If btrfs reports a stats row whose devid is outside `still_relevant` and has non-zero counters, the alert pipeline loops:

1. `compute_alert_state` sees the stale row, finds no acked baseline (reconcile just pruned it), emits `BtrfsDeviceErrors { devid: stale }`.
2. Operator runs `braid ack`. `snapshot_current` (alert.rs:161-198, called from ack.rs:93) writes an acked entry for the stale devid because it too walks every row.
3. Next monitor cycle: reconcile prunes the entry; compute re-emits the cause; latched again.

The operator sees `BtrfsDeviceErrors devid 99` for a devid that is not in `pool.json` and not in `btrfs filesystem show`. They cannot inspect it, cannot clear it.

Existing test `stale_mapper_row_no_longer_latches_computation_error` (monitor.rs:271-287) pins only the zero-counter half via `STATS_WITH_STALE_MAPPER` (test_fixtures/monitor.rs:54-61, all counters zero). The non-zero half is uncovered.

Outcome: align both passes on a single "recognized devid" set published by `AlertPoolState`, so neither writes nor inspects rows for devids outside it.

## Design

### API shape

Add a new parameter `recognized_devids: &[u64]` to both `compute_alert_state` and `snapshot_current`. Do **not** merge it with `missing_devids` -- they drive different loops:

- `missing_devids` (= null + missing) drives the `MissingDevice` cause loop and the `missing_acked = true` write loop.
- `recognized_devids` (= present + null + missing) is a *filter* over the stats-row loop.

Folding them would force every caller to recompute a subset internally and would push membership logic out of the helpers that own it. Adding one parameter is the smallest local change.

```rust
pub fn compute_alert_state(
    current_stats: &BtrfsDeviceStatsOutput,
    acked: &AckedStats,
    recognized_devids: &[u64],
    missing_devids: &[u64],
    smartd_alert_active: bool,
) -> AlertState;

pub fn snapshot_current(
    current_stats: &BtrfsDeviceStatsOutput,
    recognized_devids: &[u64],
    missing_devids: &[u64],
) -> AckedStats;
```

Inside each, the stats-row loop gains one extra skip after the existing `MissingDisk` skip:

```rust
let recognized: BTreeSet<u64> = recognized_devids.iter().copied().collect();
for dev in &current_stats.devices {
    if matches!(dev.target, DeviceStatsTarget::MissingDisk) { continue; }
    if !recognized.contains(&dev.devid) { continue; }
    // existing body
}
```

### Single source for the recognized set

Add a `recognized_devids()` method to `AlertPoolState` (`cli/src/probe.rs:264-283`) mirroring the existing `alert_missing_devids()` accessor (`cli/src/probe.rs:274-282`):

```rust
/// Devids monitor and ack treat as known pool members for the current
/// cycle: present, null-underlying, or btrfs-MISSING. Sorted, deduped.
/// Mirrors `alert_missing_devids` but unions in `present_devids`.
pub fn recognized_devids(&self) -> Vec<u64> {
    self.present_devids
        .iter()
        .copied()
        .chain(self.null_underlying.iter().map(|d| d.devid))
        .chain(self.missing_devids.iter().copied())
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect()
}
```

`BTreeSet` is already imported (`cli/src/probe.rs:1`). Both `cmd_monitor` and `cmd_ack` swap their inlined unions for this method.

## Files to modify

### `cli/src/probe.rs`
- Add `recognized_devids` method after line 282.
- Add a unit test next to `probe_pool_alerts_alert_missing_devids_method` (probe.rs:2117) confirming union and dedup of `present + null + missing`.

### `cli/src/alert.rs`
- `compute_alert_state` (lines 102-137): add `recognized_devids: &[u64]` ahead of `missing_devids`; add membership skip at the top of the stats-row loop.
- `snapshot_current` (lines 161-198): same parameter addition and same skip.
- Migrate every existing `compute_alert_state` test call (lines 998, 1010, 1020, 1030, 1054, 1078, 1094, 1104, 1171, 1194, 1209) and every `snapshot_current` test call (lines 1115, 1142, 1221) to pass an appropriate recognized slice. Pattern: for tests that exercise devid `n`, pass `&[n]`; for tests with mixed devids, pass the union. The existing `unknown_devid_zero_counters_does_not_alert` test at alert.rs:1190-1197 should pass `&[99]` to preserve its meaning (recognized devid 99 with zero counters does not alert).
- Add a new alert-level unit test `unrecognized_devid_with_errors_does_not_alert` mirroring the existing `unknown_devid_zero_counters_does_not_alert` at line 1190 but with non-zero counters and `recognized_devids = &[]` so devid 99 is unrecognized. Pin: `compute_alert_state` returns an inactive `AlertState`.

### `cli/src/monitor.rs`
- Replace the inlined union at lines 102-107 with `let recognized = pool.recognized_devids();`.
- Build the BTreeSet reconcile needs from `recognized`: `let still_relevant: BTreeSet<u64> = recognized.iter().copied().collect();` (keep `present_devids: BTreeSet<u64>` local for reconcile's `present` arg, derived from `pool.present_devids`).
- Pass `&recognized` to `compute_alert_state` at line 117, ahead of the existing `&alert_missing_devids`.
- Add new test `stale_mapper_row_with_errors_does_not_latch_or_loop` (text below).

### `cli/src/ack.rs`
- Compute `let recognized_devids = pool.recognized_devids();` near line 89.
- Pass `&recognized_devids` to `snapshot_current` at line 93, ahead of `&alert_missing_devids`.
- Add new mounted-ack regression test `cmd_ack_does_not_persist_unrecognized_devid_in_acked_stats` (text below) in `mod tests` alongside the existing mounted-ack tests at ack.rs:347 / ack.rs:392.

### `cli/src/test_fixtures/monitor.rs`
- Add `STATS_WITH_STALE_MAPPER_ERRORS` after `STATS_WITH_STALE_MAPPER` (line 61). Same shape: rows for devid 1 and 2 with zero counters, third row for devid 99 path `/dev/mapper/braid-stale` with non-zero `read_io_errs` and `corruption_errs`.
- Add `MonitorTestRunner::with_stale_mapper_errors()` constructor mirroring `with_stale_mapper_stats()` (test_fixtures/monitor.rs:99-104) but using the new payload.

### `cli/src/test_fixtures/ack.rs`
- Add a private helper `btrfs_device_stats_with_stale_devid()` mirroring `btrfs_device_stats_healthy()` (test_fixtures/ack.rs:195-221) -- same rows for devid 1 and 3 (the canonical ack-test pool), plus a third row for devid 99 path `/dev/mapper/braid-stale` with non-zero `read_io_errs` and `corruption_errs`. The existing ack fixtures use devids 1 and 3, so the stale-devid test must match (the finding's literal "1 and 2" was illustrative; the recognized set in the existing ack scaffolding is `{1, 3}`).
- Add `ack_mounted_probe_runner_with_stale_devid_stats()` mirroring `ack_mounted_probe_runner_with_device_stats()` (test_fixtures/ack.rs:247-254) but composed against the new stats helper.

### `docs/decisions/014-alerts.md`
- Update the "Ack state keyed by btrfs devid" section at lines 53-55. The current text "devid is btrfs-native -- no membership cross-reference is needed for alert counter baselines." is left misleading by this fix: baseline *keying* remains devid-native, but the alert pipeline now *does* cross-reference pool membership when deciding which stats rows to consume.
- Replace lines 53-55 with text that states (1) baselines are still keyed by btrfs devid (no path-mapping required), and (2) stats rows are filtered to the current recognized devid set (`AlertPoolState::recognized_devids` = present + null-underlying + btrfs-MISSING) before either `compute_alert_state` emits causes or `snapshot_current` writes baselines, so a stale stats row whose devid is not in the recognized set cannot latch `BtrfsDeviceErrors` or persist an ack baseline. Cross-reference the new method by name and link `cli/src/probe.rs` to anchor future readers.
- Suggested replacement text:

  > ### Ack state keyed by btrfs devid
  >
  > Acked baselines are keyed by btrfs devid (`acked-stats.json` maps stringified devid to baseline) -- no path or LUKS UUID mapping is required to associate a stats row with its baseline. The parser captures missing device devids from MISSING sentinel lines.
  >
  > Membership cross-reference is performed at the alert-pipeline boundary, not at the baseline-keying level. `AlertPoolState::recognized_devids` (`cli/src/probe.rs`) returns the union of `present_devids`, `null_underlying`, and `missing_devids` for the current cycle. Both `compute_alert_state` and `snapshot_current` filter `btrfs device stats` rows against that set before emitting causes or writing baselines. A stats row whose devid is outside the recognized set is treated as transient/stale identity: it cannot latch `BtrfsDeviceErrors`, and `braid ack` does not persist a baseline for it (which would loop on the next monitor cycle's `reconcile_acked_stats` prune).

## New monitor test

```rust
// Intent: a stats row whose devid is not in the pool's recognized set must
//   not latch a BtrfsDeviceErrors cause even when it carries non-zero
//   counters, and a follow-up monitor cycle must remain Ok -- no
//   ack-induced loop.
// Why it exists: the prior fix
//   (stale_mapper_row_no_longer_latches_computation_error) only covered
//   zero-counter stale rows. A non-zero counter row used to flow into
//   compute_alert_state, latch BtrfsDeviceErrors { devid: stale }, and --
//   once the operator ran braid ack -- snapshot_current would write an
//   acked entry that the very next monitor cycle's reconcile_acked_stats
//   would prune, re-firing the alert forever. Both passes must agree on
//   which devids matter.
// Scenario: btrfs device stats reports two healthy rows for devid 1 and 2
//   plus a stale /dev/mapper/braid-stale row at devid 99 with non-zero
//   read_io_errs / corruption_errs (lingering from a prior pool
//   configuration). Probe sees only devid 1 and 2 as present. monitor
//   must return Ok and write no alert latch; a second monitor cycle on
//   the same state must also return Ok.
#[test]
fn stale_mapper_row_with_errors_does_not_latch_or_loop() {
    let (_dir, paths) = isolated_paths();
    let runner = MonitorTestRunner::with_stale_mapper_errors();

    let first = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
    assert_eq!(
        first,
        MonitorResult::Ok,
        "non-zero counters on an unrecognized devid must not latch an alert"
    );
    assert!(
        !paths.alert_latch_json().exists(),
        "no alert latch must be written for a stale-devid row"
    );

    let runner2 = MonitorTestRunner::with_stale_mapper_errors();
    let second = cmd_monitor(&runner2, &monitor_fs_btrfs(), &monitor_mp(), &paths);
    assert_eq!(
        second,
        MonitorResult::Ok,
        "second cycle must remain Ok -- no reconcile/compute oscillation"
    );
    assert!(
        !paths.alert_latch_json().exists(),
        "no alert latch must appear on the second cycle either"
    );
}
```

Two-cycle structure pins the loop property directly: the first cycle proves no spurious latch; the second proves the absence of an ack-then-loop cycle even without an intervening `braid ack` (because the reconcile-then-compute disagreement was the loop's engine, not ack itself).

## New ack test

```rust
// Intent: cmd_ack must not persist an acked entry for a btrfs device-stats
//   row whose devid is outside the pool's recognized set, even when that
//   row carries non-zero counters. After ack, acked-stats.json contains
//   only the recognized devids' baselines.
// Why it exists: snapshot_current used to walk every stats row, so an
//   unrecognized devid 99 would land in acked-stats.json. The very next
//   monitor cycle would prune devid 99 via reconcile_acked_stats and
//   compute_alert_state would re-latch BtrfsDeviceErrors { devid: 99 } --
//   the loop the operator could never escape. Filtering snapshot_current
//   by recognized_devids closes that half of the loop; this test pins it
//   directly so an implementation that only filters compute_alert_state
//   (and leaves snapshot_current unfiltered) cannot pass.
// Scenario: a MissingDevice alert is already latched. btrfs filesystem
//   show reports devids 1 and 3 as the pool. btrfs device stats reports
//   rows for devids 1, 3, and a stale /dev/mapper/braid-stale at devid 99
//   with non-zero counters. The operator runs braid ack. ack must succeed
//   and acked-stats.json must contain keys "1" and "3" but not "99".
#[test]
fn cmd_ack_does_not_persist_unrecognized_devid_in_acked_stats() {
    let (_dir, paths) = isolated_paths();
    ack_write_latch(&paths, vec![AlertCause::MissingDevice { devid: 7 }]);
    let runner = ack_mounted_probe_runner_with_stale_devid_stats();
    let beeper = || {};

    let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);
    assert!(result.is_ok(), "ack must succeed, got {result:?}");

    let acked = alert::load_acked_stats(&paths);
    let keys: Vec<&str> = acked.0.keys().map(String::as_str).collect();
    assert!(
        keys.contains(&"1") && keys.contains(&"3"),
        "recognized devid baselines must be persisted, got {keys:?}"
    );
    assert!(
        !keys.contains(&"99"),
        "unrecognized devid must not be persisted, got {keys:?}"
    );
}
```

This is the behavioral lock for the ack half of the fix. Without filtering `snapshot_current`, the assertion `!keys.contains(&"99")` fails -- which proves the test pins the regression even if a future implementation removed only the `compute_alert_state` filter.

## Order of operations

Each numbered step compiles and tests on its own; commits can be split at any boundary.

1. **probe.rs** -- add `recognized_devids` method plus its unit test. Pure addition; nothing else compiles against it yet.
2. **alert.rs + monitor.rs + ack.rs** -- one atomic step: add the new parameter to both alert functions, migrate test call sites, thread `pool.recognized_devids()` through both production callers. Must land together; the API change breaks compilation otherwise.
3. **test_fixtures/monitor.rs + monitor.rs (test) + test_fixtures/ack.rs + ack.rs (test)** -- add the two new fixtures and the two new regression tests (one monitor, one ack).
4. **docs/decisions/014-alerts.md** -- update the "Ack state keyed by btrfs devid" section so the ADR reflects the new membership-cross-reference behavior at the alert-pipeline boundary. Documentation-only; safe to land with step 3 or as a follow-up.

If splitting across commits: step 1 alone; step 2 together; steps 3-4 as a follow-up.

## Risk check: does this hide real disk errors?

A real-disk error is hidden only if a btrfs stats row has a devid the kernel reports as a live FS member, while `probe_pool_alerts` does not surface that devid into `present + null + missing`.

- `btrfs device stats` and `btrfs filesystem show` both enumerate the kernel's `fs_info` device list (`reference/btrfs-progs/cmds/device.c:935` and the underlying ioctls). At the kernel level the two cannot disagree about FS membership.
- `present_devids` is built from every `/dev/mapper/...` entry in `btrfs filesystem show` whose cryptsetup mapper is active with a non-null backing (probe.rs:317-360). Null-backed or btrfs-MISSING devids surface via `null_underlying` and `missing_devids` respectively.
- A devid in stats that probe drops would require probe to short-circuit before it filled the set. In that case `probe_pool_alerts` returns `Err`, monitor never reaches `compute_alert_state`, and a `ComputationError` latch fires instead.

The only rows the new filter discards are rows for devids btrfs itself does not consider pool members -- the artificial test scaffolding case and the narrow kernel-race case during in-progress `btrfs device add/remove/replace`. The next monitor cycle picks up real disk errors once the FS settles.

## Verification

Run from the repo root:

- `just test-rust` -- exercises every migrated test call site in `cli/src/alert.rs`, the new `AlertPoolState::recognized_devids` unit test, the new `unrecognized_devid_with_errors_does_not_alert` alert-level test, the new `stale_mapper_row_with_errors_does_not_latch_or_loop` monitor test, and the new `cmd_ack_does_not_persist_unrecognized_devid_in_acked_stats` ack test.
- `just test-rust` again, specifically scoped if desired:
  - `cargo test -p braid-cli --lib alert::tests`
  - `cargo test -p braid-cli --lib monitor::tests::stale_mapper_row_with_errors_does_not_latch_or_loop`
  - `cargo test -p braid-cli --lib monitor::tests::stale_mapper_row_no_longer_latches_computation_error` (must still pass -- zero-counter case unchanged)
  - `cargo test -p braid-cli --lib ack::tests::cmd_ack_does_not_persist_unrecognized_devid_in_acked_stats`
  - `cargo test -p braid-cli --lib probe::tests` (covers new and old AlertPoolState method tests)
- `just test-vm` -- no VM-level test changes; this just confirms the alert pipeline tests still pass end-to-end through the NixOS VM harness (alerts.py and related checks consume the same `cmd_monitor` / `cmd_ack` code paths).

Manual regression sanity checks (no production fixture required), each pins one half of the fix:
- Remove the new `recognized.contains(&dev.devid)` skip line in `compute_alert_state` and re-run tests. `stale_mapper_row_with_errors_does_not_latch_or_loop` must fail with a `MonitorResult::Alert` containing `BtrfsDeviceErrors { devid: 99 }`.
- Restore that skip and remove the same line in `snapshot_current`. `cmd_ack_does_not_persist_unrecognized_devid_in_acked_stats` must fail because `acked-stats.json` will contain `"99"`.

## Critical files

- `/Users/dan/Code/braid/cli/src/probe.rs` -- new `recognized_devids` method + test
- `/Users/dan/Code/braid/cli/src/alert.rs` -- API extension on `compute_alert_state` and `snapshot_current`, body filter, ~14 test migrations, 1 new alert-level test
- `/Users/dan/Code/braid/cli/src/monitor.rs` -- caller migration, 1 new test pinning the no-loop property
- `/Users/dan/Code/braid/cli/src/ack.rs` -- caller migration, 1 new test pinning the snapshot-side filter
- `/Users/dan/Code/braid/cli/src/test_fixtures/monitor.rs` -- new `STATS_WITH_STALE_MAPPER_ERRORS` constant and `with_stale_mapper_errors()` builder
- `/Users/dan/Code/braid/cli/src/test_fixtures/ack.rs` -- new `btrfs_device_stats_with_stale_devid()` helper and `ack_mounted_probe_runner_with_stale_devid_stats()` runner
- `/Users/dan/Code/braid/docs/decisions/014-alerts.md` -- rewrite "Ack state keyed by btrfs devid" section so the ADR documents the new membership-cross-reference at the alert-pipeline boundary
