# Plan: eliminate the redundant post-loop probe in live-pool add

## Context

The live-pool branch of `AddPlan::execute` probes the pool N+1 times for
N added devices: once per loop iteration (for devid lookup and ack
cleanup) and once again immediately after the loop (for the membership
persistence snapshot). The post-loop probe at `cli/src/add.rs:1302` is
observationally redundant: between the last per-target probe and the
post-loop probe there are no mutating calls -- only
`find_added_device_by_uuid` (pure) and
`alert::drop_ghost_acked_for_devids` (writes
`acked-stats.json`, not pool state). The last loop iteration's
`pool_after` therefore captures the same post-add btrfs view that the
second probe would.

Goal: drop the post-loop probe, hoist `pool_after` out of the loop
scope, and reuse the last iteration's snapshot. Saves one btrfs +
cryptsetup round-trip per live-pool add invocation and removes the
"why two snapshots?" question every reader of `AddPlan::execute` has
to answer.

The bigger pivot of "single probe + bulk ack cleanup after the loop"
is **not** an option: the test
`cmd_add_partial_multi_add_cleans_succeeded_disk_before_later_failure`
at `cli/src/add.rs:4416` explicitly pins the per-target cleanup
boundary so disk2's reused devid baseline is gone before a later
disk3 add can fail.

## File to modify

- `cli/src/add.rs` -- only the live-pool `else` branch at lines
  1273-1316 changes. The bootstrap branch (`if !self.pool.mounted`,
  lines 1233-1272) is untouched.

## Change

Inside the `else { /* Add each to existing pool */ }` arm:

1. Declare `let mut pool_after: Option<PoolState> = None;` immediately
   before the `for target in &needs_pool_add` loop.
2. Inside the loop, rename the existing `let pool_after = probe_pool(...)?`
   binding to `let probe = probe_pool(...)?` (or keep the existing name
   inside a new block-scoped binding; the simpler form is to rename).
   Use `probe` for `find_added_device_by_uuid` and the
   `drop_ghost_acked_for_devids` devid lookup as today. At the end of
   the iteration, assign `pool_after = Some(probe);`.
3. After the loop, replace `let pool_after = probe_pool(runner, fs, mount_point)?;`
   with `let pool_after = pool_after.expect("needs_pool_add is non-empty in the live-pool branch (journal_targets.is_empty() guard at line 1014, and journal_targets and needs_pool_add are populated in lockstep)");`
4. Update the surrounding comment block at lines 1299-1305: the
   "Distinct from the per-target probe above" justification no longer
   applies. Replace with one short line noting the snapshot comes from
   the last per-target probe, then the existing UUID-presence
   validation loop and `enrich_from_pool_state` + `save_membership`
   calls run unchanged.

Per-target error mapping inside the loop stays exactly as today:
`AddError::AckCleanupFailed { stage: "post-add probe", detail: format!("{}: {e}", target.name) }`
on probe failure, and the existing `AckCleanupFailed { stage: "post-add probe", ... }`
on missing mapper, and `AckCleanupFailed { stage: "live-pool add", ... }`
on `drop_ghost_acked_for_devids` failure.

### Invariant justifying `expect`

The `expect` is safe because:

- Line 1014: `if journal_targets.is_empty() { return Ok(()); }` short-circuits before reaching the live-pool branch.
- `journal_targets` and `needs_pool_add` are populated in lockstep across all paths in `execute`:
  - OpenRecoverable: both pushed (lines 929 + initial set).
  - ClosedPresentLuks + BraidLabeledRecoverable: both pushed (lines 997-1008).
  - ClosedPresentLuks + BraidLabeledAlreadyInPool: neither pushed (`continue` at line 976).
  - Fresh (Pass 2): both pushed (initial set + line 1168).
- Therefore `journal_targets` non-empty implies `needs_pool_add` non-empty, the loop runs >= 1 iteration, and `pool_after` is `Some`.

The `expect` message should reference this invariant inline so the
next reader does not have to re-derive it.

## Types and helpers reused

- `PoolState` is defined at `cli/src/types.rs:350-351` with
  `#[derive(Debug, Clone, PartialEq, Eq)]`. `Option<PoolState>` works
  without any new derives.
- `probe_pool` at `cli/src/probe.rs:378` is the existing helper. No
  signature change.
- `find_added_device_by_uuid` at `cli/src/add.rs:777` and
  `alert::drop_ghost_acked_for_devids` at `cli/src/alert.rs:221` are
  used as-is.
- `AddError::AckCleanupFailed` variant is reused without change.

## Tests

No new tests required. Existing coverage that should keep passing:

- `cmd_add_post_add_probe_uncertainty_is_fatal` (`cli/src/add.rs:4587`)
  -- single-disk add; the runner's `fail_post_add_probe` flag fires
  the moment a mapper has been added, which is exactly when the
  hoisted per-target probe runs. Error variant and `stage`
  ("post-add probe") are preserved.
- `cmd_add_partial_multi_add_cleans_succeeded_disk_before_later_failure`
  (`cli/src/add.rs:4416`) -- pins per-target cleanup boundary; the
  hoist leaves per-target probe + cleanup intact, so disk2's ghost
  still gets dropped before disk3's add fails.
- `cmd_add_live_pool_acked_cleanup_parse_failure_is_fatal`
  (`cli/src/add.rs:4479`) -- pins `live-pool add` stage on cleanup
  failure; unchanged.
- `cmd_add_bootstrap_warns_when_post_mount_probe_errors` -- touches
  only the bootstrap branch, which this plan does not modify.

Probe-call counts are not asserted in any add test (verified by
search for `probe_count`, `call_count`, `invocations`,
`assert.*BtrfsFilesystemShow` -- only `replace.rs`, `enroll_key_file.rs`,
and `lock.rs` count probes, and they count different commands on
different paths).

## Verification

1. `just test-rust` -- runs `cargo test`; must pass with no new
   failures. Watch specifically for the four tests listed above.
2. `just test-vm add` (or the relevant add-related VM check name) --
   exercises the live live-pool add path end-to-end against real
   btrfs and cryptsetup. The probe-count change is invisible at this
   layer, but it confirms no regression in the per-target ack
   cleanup or the post-loop membership commit.
3. Manual code re-read of `cli/src/add.rs:1273-1316` after the edit
   to confirm:
   - The `expect` message names the invariant.
   - The post-loop `probe_pool` call is gone.
   - The per-target error mappings are unchanged.
   - The comment above the membership commit no longer says
     "Distinct from the per-target probe above".

## Out of scope

- The bootstrap branch (lines 1233-1272) keeps its single
  post-mount probe -- there is no loop and no redundancy.
- `replace.rs` and `remove.rs` are not touched; their probe patterns
  are already single-shot.
- No changes to `probe_pool`, `find_added_device_by_uuid`, or
  `alert::drop_ghost_acked_for_devids` signatures or behavior.
