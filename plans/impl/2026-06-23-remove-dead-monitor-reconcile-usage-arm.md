# Plan: remove the dead `BtrfsDeviceUsageRaw` arm from `MonitorReconcileRunner`

## Context

`MonitorReconcileRunner::run` in `cli/src/test_fixtures/monitor.rs` carries a
`BtrfsDeviceUsageRaw` match arm that serves a healthy usage payload, with a
comment admitting the arm is "unreached -- present defensively." It is provably
dead under this runner's topology:

- The runner always serves `BTRFS_SHOW_PRESENT_NULL_MISSING` (devid 3 `MISSING`,
  `Total devices 3`), so `probe_pool_alerts` reports `missing_count >= 1`
  (`cli/src/probe.rs#probe_pool_alerts`, count derived at probe.rs:516; the canary
  `probe_pool_mounted_with_missing` asserts `missing_count == 1` for the identical
  shape).
- `cmd_monitor` passes that `missing_count` to `evaluate_enospc_for_monitor`,
  which returns `None` at `cli/src/monitor.rs:226-228` (`if missing_count > 0`)
  *before* calling `probe_usage_entries` (monitor.rs:194) -- the sole issuer of
  `CmdRequest::BtrfsDeviceUsageRaw` in the monitor path. `probe_pool_alerts`
  issues no usage request.

So the arm can never execute for either consuming test. Two problems with leaving
it:

1. **Misleading.** The "defensive" framing implies the reconcile tests might
   exercise the ENOSPC usage path; they never do. A future reader can be misled
   into thinking ENOSPC is covered here.
2. **Silently wrong on regression.** If a future change made the monitor probe
   usage on a degraded pool (a real bug -- it would touch the ENOSPC baseline the
   monitor must leave untouched while degraded), this runner would silently serve
   a plausible payload and the test would still pass, masking the regression.

The intended outcome: the runner fails loud on an unexpected usage probe, matching
the file's own idiom and the project-wide fail-closed fixture convention, with the
dead payload and misleading comment gone.

## Change

In `cli/src/test_fixtures/monitor.rs`, `impl CommandRunner for MonitorReconcileRunner`,
delete the `BtrfsDeviceUsageRaw` arm and its 4-line comment, and replace it with a
short comment above the existing catch-all explaining the deliberate omission.

Before (monitor.rs:388-394):

```rust
            CmdRequest::BtrfsDeviceStatsJson { .. } => Ok(ok_output(STATS_2DISK_HEALTHY)),
            // Healthy usage payload. This runner's topology is degraded (devid 3
            // MISSING), so the monitor skips ENOSPC before probing usage and this
            // arm is unreached -- present defensively so a future non-degraded
            // reconcile fixture cannot panic here.
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(ok_output(&usage_2disk_healthy())),
            other => panic!("unexpected CmdRequest in monitor reconcile test: {other:?}"),
```

After:

```rust
            CmdRequest::BtrfsDeviceStatsJson { .. } => Ok(ok_output(STATS_2DISK_HEALTHY)),
            // No BtrfsDeviceUsageRaw arm: this runner's degraded topology (devid 3
            // MISSING) makes the monitor skip ENOSPC before the usage probe, so a
            // usage request here is a bug -- let it hit the catch-all panic below.
            other => panic!("unexpected CmdRequest in monitor reconcile test: {other:?}"),
```

That is the entire change: one match file, net -2 lines.

## Why this shape (delete + catch-all, not an explicit panic arm)

The cited finding proposed replacing the arm with `panic!("unexpected
BtrfsDeviceUsageRaw in reconcile test")`. Deleting and relying on the existing
catch-all is simpler and strictly better here:

- **It matches the file's own idiom.** Both runners in this file enumerate only
  the commands they serve and send everything else to `other => panic!(...)`.
  Neither has a per-command panic arm. An explicit `BtrfsDeviceUsageRaw => panic!`
  would introduce a one-off pattern used nowhere else in the file.
- **The catch-all message is richer.** `unexpected CmdRequest in monitor reconcile
  test: {other:?}` expands to the full request debug including `mount_point`,
  versus the hand-written string in the proposal.
- **A one-line comment preserves the "why".** Because the sibling `MonitorTestRunner`
  *does* serve usage, the absence of a usage arm here is non-obvious; the replacement
  comment documents the deliberate omission so a future maintainer does not re-add a
  silent payload. This honors the file's heavy "why"-comment culture without keeping
  behavior-redundant code.

This also aligns with the project-wide convention that "must never be called"
fixture branches fail loud: `unreachable!`/`panic!` in `cli/src/test_fixtures/{ack,
idle,doctor,status}.rs` and `MonitorFs`'s own probe methods in this same file. The
silent-serve arm was the lone outlier.

No follow-on cleanup: `usage_2disk_healthy()` and `ok_output` remain used by
`MonitorTestRunner`, so removing this call leaves no dead helper and no unused
import.

## Files

- `cli/src/test_fixtures/monitor.rs` -- delete the `BtrfsDeviceUsageRaw` arm +
  comment in `MonitorReconcileRunner::run`; add the one-line omission comment above
  the catch-all.

## Verification

The two tests that use `MonitorReconcileRunner` are
`cmd_monitor_reconciles_acked_stats_across_pool_axes` and
`save_acked_stats_failure_latches_computation_error` (both in
`cli/src/monitor.rs` `mod tests`). They must still pass -- and their passing *is*
the proof the arm was dead: if the monitor reached the usage probe, the catch-all
panic would now fire and these tests would fail.

1. Targeted -- run each reconcile test, then the broader monitor suite. `cargo
   test` takes only one positional filter before `--`, so the two named tests are
   separate invocations:
   ```
   cargo test --manifest-path cli/Cargo.toml --lib cmd_monitor_reconciles_acked_stats_across_pool_axes
   cargo test --manifest-path cli/Cargo.toml --lib save_acked_stats_failure_latches_computation_error
   cargo test --manifest-path cli/Cargo.toml --lib monitor::
   ```
2. Full canonical Rust suite (project recipe):
   ```
   just test-rust
   ```
3. Lint -- confirm no unused-helper / dead-code warning was introduced:
   ```
   cargo clippy --manifest-path cli/Cargo.toml --tests
   ```

All three should pass with no new warnings. (The replacement comment is ASCII-only
and lives in a comment, so it is exempt from and clean under
`scripts/docs/check-output-ascii.py`.)
