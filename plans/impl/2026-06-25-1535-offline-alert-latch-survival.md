# Plan: pin alert-latch survival across an offline monitor cycle

## Context

`cmd_monitor` classifies an offline pool by returning `Ok(None)` early --
`if !pool.mounted { return Ok(None); }` (`cli/src/monitor.rs:83-85`), folded to
`MonitorResult::PoolOffline` at `cli/src/monitor.rs:149`. That early return fires
*before* the latch load / merge / save (steps 8-11, `monitor.rs:158-177`). ADR 014's
sticky-latch invariant -- "all cause types persist until `braid ack`, even if the
triggering condition resolves" (`docs/design/decisions/014-alerts.md`) -- therefore
rests, on the offline path, entirely on that early return leaving `alert-latch.json`
untouched.

The three existing offline tests -- `monitor_classifies_unmounted_as_offline`,
`monitor_classifies_non_btrfs_mount_as_offline`, `cmd_monitor_offline_pool_ignores_smartd_flag`
-- all start with **no** latch on disk and only assert one is not *created*. None pin
that an already-active latch *survives* an offline cycle.

Why that gap bites: a refactor that moved the latch load above the early return, or
called `alert::remove_alert_latch` on the offline path -- which `cmd_ack` legitimately
does for a genuine offline ack (`cli/src/ack.rs`, `remove_alert_latch` /
`remove_alert_latch_corrupt`) -- would compile and pass every existing monitor test
while silently dropping an in-flight alert. The beeper keeps sounding (the monitor
wrapper only ever *starts* `braid-alert.service` on exit 1 and never stops it; only
`braid ack` stops it -- `modules/braid/monitor.nix`), but `braid status` reads the
latch directly and would go quiet. Net effect: a fail-open display regression.

This is pure test hardening -- the current code is correct; no behavior changes.

## Approach

Add **one** behavioral regression test to `cli/src/monitor.rs` `mod tests`, pinning
the invariant at its realistic trigger -- the "pool unmounts mid-incident" not-mounted
path -- with a structure-insensitive assertion: seed an active latch, run an offline
cycle, assert the latch's on-disk bytes are unchanged and it still reloads to the
seeded alert.

**Why only the not-mounted arm (deliberate, not a shortcut):** both offline arms
(`!pool.mounted` and `ProbeError::NotBtrfs` at `monitor.rs:64`) share one latch
contract -- "leave the file untouched" -- and converge on the same `Ok(None)` handler.
The existing per-arm offline tests are justified because each pins a *distinct*
behavior (NotBtrfs-vs-beep classification; no-entry-vs-IO-failure). Latch survival has
no such behavioral fork, so a second test on the NotBtrfs arm would re-confirm the same
behavior through a second internal branch -- coverage fitted to code *structure* rather
than behavior, which is the kind of test braid's bar declines. The realistic regression
(`remove_alert_latch` mirroring ack's offline path) lands in the shared `Ok(None)`
handler and is caught here.

## The test

Location: `cli/src/monitor.rs`, in `mod tests`, beside `monitor_classifies_unmounted_as_offline`.

Shape mirrors two existing tests: `healthy_cycle_carries_forward_existing_non_computation_latch`
(latch seed + `MissingDevice { devid: 7 }` choice) and
`cmd_monitor_corrupt_acked_stats_latches_computation_error` (the byte-identity
before/after read).

```rust
// Intent: an already-active alert latch survives a monitor cycle that concludes
//   PoolOffline -- the early Ok(None) return must leave alert-latch.json
//   byte-for-byte untouched, and it must still reload to the seeded alert.
// Why it exists: the PoolOffline early return (monitor.rs `if !pool.mounted`)
//   fires before the latch load/merge/save, so ADR 014's sticky-latch invariant
//   here rests entirely on that path NOT touching the file. Every existing offline
//   test starts with no latch and only asserts none is created. A refactor that
//   moved the latch load above the early return, or called alert::remove_alert_latch
//   on the offline path -- as cmd_ack legitimately does for a genuine offline ack --
//   would compile and pass every other monitor test while silently dropping an
//   in-flight alert: the beeper keeps sounding (monitor never stops it) while
//   `braid status` goes quiet, a fail-open display regression.
// Scenario: a prior cycle latched MissingDevice { devid: 7 } and the beeper is
//   sounding; the operator's pool briefly unmounts (or `braid monitor` is run by
//   hand while offline) so the next cycle sees an empty mountinfo.
#[test]
fn unmounted_pool_preserves_existing_alert_latch() {
    let (_dir, paths) = isolated_paths();
    let existing = alert::AlertState {
        causes: vec![AlertCause::MissingDevice {
            devid: Devid::new(7),
        }],
    };
    alert::save_alert_latch(&existing, &paths).unwrap();
    let before = std::fs::read(paths.alert_latch_json()).unwrap();

    let result = cmd_monitor(
        &MonitorTestRunner::with_stale_mapper_stats(),
        &monitor_fs_not_mounted(),
        &monitor_mp(),
        &paths,
    );

    assert_eq!(result, MonitorResult::PoolOffline);
    let after = std::fs::read(paths.alert_latch_json()).unwrap();
    assert_eq!(
        after, before,
        "an offline cycle must leave an active alert latch byte-for-byte untouched"
    );
    // Semantic check: the latch still reloads to the seeded alert.
    assert_eq!(
        alert::load_alert_latch(&paths).unwrap().unwrap(),
        existing,
        "the latched MissingDevice alert must survive the offline cycle"
    );
}
```

**No new imports / helpers / fixtures.** All of `alert`, `AlertCause`, `Devid`,
`MonitorResult`, `MonitorTestRunner`, `monitor_fs_not_mounted`, `monitor_mp`,
`isolated_paths`, and `std::fs` are already in scope in `mod tests`. The runner is
never actually invoked (the early return precedes any btrfs command), so
`with_stale_mapper_stats()` is chosen only for consistency with the sibling offline
tests.

## Why byte-identity (not only a reload compare)

`std::fs::read` before/after pins "not removed AND not rewritten," matching the
precedent at `cmd_monitor_corrupt_acked_stats_latches_computation_error`. It stays
structure-insensitive: removal makes the second read `unwrap` panic, a clobber makes
the bytes differ -- both fail; a refactor that preserves the latch through some other
mechanism still passes. The trailing `load_alert_latch` compare documents intent (the
alert is still latched) without weakening the byte check.

## Verification

- `cargo test -p braid-cli unmounted_pool_preserves_existing_alert_latch` (or
  `just test-rust`) -- new test passes.
- Confirm it fails for the right reason: temporarily insert
  `let _ = alert::remove_alert_latch(&paths);` immediately before
  `return MonitorResult::PoolOffline` at `monitor.rs:149`, re-run; the test must fail
  at the second `std::fs::read(...).unwrap()` (file removed). Revert.
- `just test-rust` full suite stays green.

No fixture refresh (no parser/nixpkgs change), no `flake.nix` `checks` registration
(this is a Rust unit test inside an existing module, not a VM test), and no docs change
(behavior is unchanged).
