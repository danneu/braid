# Extend acked-stats hygiene to `cmd_recover`

## Context

Commit `92ad988` (plan `plans/impl/2026-04-28-acked-stats-hygiene.md`) installed a
three-layer policy to prevent reused-devid alert suppression:

1. Correctness boundary -- `cmd_add` clears acked-stats on bootstrap
   (`add.rs:1050`, `remove_acked_stats`) and drops each newly-assigned devid in
   the live-add loop (`add.rs:1085`, `drop_ghost_acked_for_devids`). Failure is
   command-fatal via `AddError::AckCleanupFailed`.
2. Hygiene -- `cmd_remove` (`remove.rs:356`) and `cmd_remove_missing`
   (`remove_missing.rs:284`) drop the affected devid on success. Failure is
   warning-only.
3. Defense-in-depth -- `cmd_monitor` (`monitor.rs:114`) reconciles each cycle.

The plan did not extend the boundary to `cmd_recover`. Today, an
interrupted-then-recovered add (bootstrap **or** live-add) finishes the btrfs
mutation but leaves `acked-stats.json` untouched. Monitor's reconcile cannot
close the gap, because for the cases this plan targets the stale entry's devid
is still in `still_relevant` (bootstrap: old pool's devid 1 == new pool's
devid 1; live-add devid reuse: btrfs reused the just-removed max devid). A
SMART/btrfs alert acked under that devid on the old physical disk is silently
inherited by the new disk until counters exceed the ghost baseline.

Replace is intentionally out of scope: live `replace.rs` itself never touches
acked-stats because btrfs replace preserves the devid in-place, so the existing
baseline maps to the same logical slot and `exceeds_acked` self-resets when
`current < acked`. Recovery must mirror that.

Intended outcome: each recovery handler that finishes an op whose live path
touches acked-stats now calls the same helper at the same logical point, with
the same failure policy (fail-closed for add, warning for remove*). Two
recovery-specific invariants get explicit treatment:

- **Live-add recovery covers the "committed-but-closed" target** -- a target
  whose `pool_add_device` completed in btrfs before the crash bypasses
  per-arm cleanup via two skip paths: (a) the outer `add_targets_all_live`
  gate at `recover.rs:2091/2124` skips the entire replay loop when every
  target is live at entry; (b) the per-iteration `continue` inside the
  replay loop (`recover.rs:2158`) skips individual targets already in the
  live pool while still replaying the rest. Per-arm cleanup alone misses
  both. A sweep over all journaled targets, placed before each
  save_membership callsite (post-replay and all-live), closes both gaps.
- **Remove recovery is commit-gated** -- `execute_generic_live_pool_recovery`
  also handles "remove did not complete" states where the target is still in
  the pool (`recover.rs:965-980`). Cleanup must run only when the recovered
  membership no longer contains the target name; an unconditional drop would
  remove a legitimate baseline for a live disk.

## Approach

One helper call per recovery branch, mirroring the live path. Failure policy
matches the live path it mirrors:

| Branch | Recovery handler | Live path helper | Recovery action | Failure |
|---|---|---|---|---|
| `Add{PoolMutation}` + `pre.is_empty()` (bootstrap) | `execute_generic_live_pool_recovery` | `remove_acked_stats` (`add.rs:1050`) | `remove_acked_stats` | fail-closed |
| `Add{PoolMutation}` + `!pre.is_empty()` (live-add) | `execute_add_pool_mutation_recovery` | per-devid `drop_ghost_acked_for_devids` (`add.rs:1085`) | per-arm inside replay loop **AND** sweep over all targets before BOTH save_membership callsites (covers all-live, mixed batch, and replay paths) | fail-closed |
| `Add{PostAddBalanceRaid1}` | `execute_add_post_balance_recovery` | already cleaned in earlier phase | no change | -- |
| `Remove{...}` (committed -- target absent from recovered) | `execute_generic_live_pool_recovery` | `drop_ghost_acked_for_devids` (`remove.rs:356`) | `drop_ghost_acked_for_devids` for `pre_membership.disks[name].devid`, gated on `!recovered.disks.contains_key(name)` | warning |
| `Remove{...}` (uncommitted -- target restored) | same handler | n/a (live `cmd_remove` never reached cleanup) | **no change** -- preserve baseline | -- |
| `RemoveMissing{PoolMutation}` | `execute_remove_missing_pool_mutation_recovery` | n/a (transitions to PostMaintenance) | none direct -- covered by PostMaintenance | -- |
| `RemoveMissing{PostRemoveMissingMaintenance}` | `execute_remove_missing_post_maintenance_recovery` | `drop_ghost_acked_for_devids` (`remove_missing.rs:284`) | `drop_ghost_acked_for_devids(&[devid])` | warning |
| `Replace{PoolMutation}` / `Replace{PostReplaceMaintenance}` | `execute_replace_*` | none (live replace does not touch acked) | **no change** -- explicit | -- |

## Changes

### 1. New `RecoverError` variant -- `cli/src/recover.rs`

Insert after `RecoverError::Failed(String)` at line 43:

```rust
#[error(
    "pool was modified by recovery, but acked-stats cleanup failed at {stage}: {detail}\n\
     pending-op.json is preserved; rm /var/lib/braid/acked-stats.json before \
     trusting `braid monitor`, then re-run `braid recover`."
)]
AckCleanupFailed { stage: &'static str, detail: String },
```

Mirrors the `AddError::AckCleanupFailed { stage, detail }` precedent
(`add.rs:1050`). The hygiene callsites (Remove, RemoveMissing PostMaintenance)
do NOT use this variant -- they `eprintln!("Warning: ...")` and continue.

### 2. Expose `devid_for_mapper_path` with doc comment -- `cli/src/add.rs:639`

Change `fn devid_for_mapper_path(...)` to `pub(crate) fn devid_for_mapper_path(...)`
and add a `///` doc comment per `AGENTS.md` "Doc Comments":

```rust
/// Mapper-path -> devid lookup shared by `cmd_add` and `cmd_recover`.
///
/// Both call sites need the canonical post-btrfs-add devid mapping to drop
/// any stale `acked-stats.json` baseline for the just-assigned devid. Keeping
/// one lookup ensures live add and add-recovery normalize mapper paths the
/// same way; divergence would let one path drop the wrong devid's ghost.
pub(crate) fn devid_for_mapper_path(pool: &PoolState, mapper_path: &str) -> Option<u64> { ... }
```

Add to `cli/src/recover.rs` imports: `use crate::add::devid_for_mapper_path;`.
Lives next to its existing behavior test at `add.rs:1999`. Moving the helper
to a shared module would churn imports for no design benefit.

### 3. Enrich Remove journal `pre_membership` with target devid -- `cli/src/remove.rs:290-306`

`cmd_remove` loads `pre_membership = membership::load_membership(...)` at line
291. The persisted `pool.json` may carry `devid: None` for any member that was
written via `DiskMember::from_by_id` (`membership.rs:66`) -- common in
discover-time bootstrap, and the shape used by `two_disk_healthy()` fixture
(`test_fixtures/shared.rs:203`). Recovery's hygiene cleanup needs the target's
devid to call `drop_ghost_acked_for_devids`; without enrichment, recovery
silently skips the cleanup for any Remove journaled against a no-devid
`pool.json`.

Between line 296 (`let mut target_membership = pre_membership.clone();`) and
line 297 (`target_membership.disks.remove(&work_plan.name);`), splice the
live-pool devid into `pre_membership` BEFORE cloning:

```rust
// Pin the target's btrfs devid into the journal so recovery (which only
// reads journal + live pool) can drop the matching acked-stats entry
// after a committed eviction. Without this, a discover-time pool.json
// (DiskMember::from_by_id, devid: None) propagates None through the
// journal and recovery silently skips hygiene cleanup.
let mut pre_membership = pre_membership;
if let Some(member) = pre_membership.disks.get_mut(&work_plan.name) {
    member.devid = Some(work_plan.target_devid);
}
let mut target_membership = pre_membership.clone();
target_membership.disks.remove(&work_plan.name);
```

`work_plan.target_devid` is in scope (`remove.rs:100`). Live `cmd_remove`'s
own cleanup at line 356 already uses `work_plan.target_devid` directly, so
the live path is unchanged. The journal enrichment is the new contract.

### 4. Bootstrap recovery -- `cli/src/recover.rs:945` (`execute_generic_live_pool_recovery`)

Between `membership::save_membership(&recovered, ...)?` at line 1002 and
`replay_post_mutation(...)?` at line 1005, insert:

```rust
// Bootstrap recovery mirrors add.rs:1050. An interrupted bootstrap landed
// here when pre_membership was empty (new pool identity). Every entry in
// acked-stats.json belongs to the wiped previous pool and must not bind to
// the freshly-assigned devids. Fail-closed for the same reason cmd_add is:
// btrfs has mutated; we cannot return success with a known-stale baseline.
if matches!(&plan.journal.op, journal::OpKind::Add { .. })
    && plan.journal.pre_membership.disks.is_empty()
{
    alert::remove_acked_stats(params.paths).map_err(|e| {
        RecoverError::AckCleanupFailed {
            stage: "bootstrap-recovery",
            detail: e.to_string(),
        }
    })?;
}
```

Placement is before `clear_journal` (line 1013) so the journal survives on
failure and the next `braid recover` retries.

### 5. Remove recovery hygiene (commit-gated) -- same `execute_generic_live_pool_recovery`

After the bootstrap block above and `replay_post_mutation` (line 1011),
before `clear_journal` (line 1013), insert:

```rust
// Hygiene -- mirrors remove.rs:356, BUT only when btrfs eviction
// committed. execute_generic_live_pool_recovery also handles
// "remove did not complete" states: lines 965-980 deliberately restore
// pre_membership disks that btrfs still owns (live, null-underlying, or
// MISSING). Skipping when `recovered.disks` still contains the target
// preserves a legitimate ack baseline for a live disk. Warning-only on
// committed branch because cmd_add is the fail-closed boundary.
if let journal::OpKind::Remove { name } = &plan.journal.op
    && !recovered.disks.contains_key(name)
    && let Some(devid) = plan
        .journal
        .pre_membership
        .disks
        .get(name)
        .and_then(|m| m.devid)
    && let Err(e) = alert::drop_ghost_acked_for_devids(params.paths, &[devid])
{
    eprintln!("Warning: failed to update acked stats: {e}");
}
```

`recovered` is in scope at this point (built at line 953 and saved at line
1002). The `name` field of `OpKind::Remove` is the only field on the variant
(`journal.rs:115-117`). The devid lookup falls back to the enriched
`pre_membership` from Change 3.

### 6. Live-add recovery (per-arm + sweep) -- `cli/src/recover.rs:2055` (`execute_add_pool_mutation_recovery`)

Two insertion points cover both crash windows:

#### 6a. Per-arm drop inside the replay loop

The replay loop at lines 2138-2280 ends each per-target iteration with
`pool_add_device(...)` (lines 2201 or 2274) followed by
`pool = probe::probe_pool(runner, fs, mount_point)?` at line 2278 and
`validate_live_members_allowed(&pool, union)?` at line 2279.

Between line 2278 (re-probe) and line 2279 (validate), insert:

```rust
// Mirrors add.rs:1085. Drop any pre-existing acked-stats entry for the
// just-assigned devid before the next iteration. Fail-closed: per-disk
// placement preserves the partial-recovery invariant -- if a later target
// fails, the earlier targets' ghosts are already cleaned. devid lookup
// uses the just-completed re-probe at line 2278.
let devid = devid_for_mapper_path(&pool, &mapper_path).ok_or_else(|| {
    RecoverError::AckCleanupFailed {
        stage: "live-pool add recovery",
        detail: format!("{name}: not found in pool after replayed add"),
    }
})?;
alert::drop_ghost_acked_for_devids(params.paths, &[devid]).map_err(|e| {
    RecoverError::AckCleanupFailed {
        stage: "live-pool add recovery",
        detail: format!("devid {devid}: {e}"),
    }
})?;
```

`mapper_path` and `name` are already in scope at this point in both arms.

#### 6b. Sweep before BOTH save_membership callsites

The replay loop has two skip paths that bypass per-arm cleanup:

1. **All-live**: when `add_targets_all_live(&pool, targets)` returns true
   at function entry (or after the initial open/scan reconcile at lines
   2091-2147), the entire replay loop at lines 2147-2306 is skipped and
   control falls through to the post-`if` save_membership at line 2311.
2. **Mixed (per-target skip)**: the replay loop at line 2147 fires but
   each iteration starts with
   `if live_member_names(&pool).contains(name) { continue; }` (at line
   2158 of the recovery file). Targets already live at the start of an
   iteration get `continue`'d and never reach the per-arm cleanup at line
   2278. After the loop completes, control reaches the post-replay
   save_membership at line 2291.

Both skip paths leave the crash window "after `pool_add_device` succeeded
in btrfs, before the live ghost-drop ran" uncovered. A single per-target
sweep that runs before each save_membership callsite closes both windows.

Place the same sweep block immediately before each save_membership call:

- **Post-replay path**: between line 2290 (`let recovered =
  build_membership_from_live_pool(...)?`) and line 2291
  (`membership::save_membership(&recovered, params.paths)?`).
- **All-live path**: between line 2310 (`let recovered =
  build_membership_from_live_pool(...)?`) and line 2311
  (`membership::save_membership(&recovered, params.paths)?`).

```rust
// Sweep cleanup for all journaled targets before phase advancement.
// Covers two crash windows the per-arm cleanup (6a) does NOT:
//   1. All-live entry: outer `if !add_targets_all_live` is false, the
//      replay loop is skipped entirely, and per-arm never runs.
//   2. Mixed batch: the replay loop fires for missing targets but the
//      `continue` at the top of each iteration skips already-live
//      targets, which never reach the per-arm site.
// `drop_ghost_acked_for_devids` is idempotent (Ok(false) when nothing
// matched), so calling it again for a devid already cleaned per-arm is
// a free no-op. Fail-closed: an unresolved devid means recovery's view
// of the pool disagrees with the journaled targets -- the correct
// response is to abort, preserve the journal, and surface the error.
let mut sweep_devids: Vec<u64> = Vec::with_capacity(targets.len());
for (name, target) in targets {
    let mapper_path = format!("/dev/mapper/{}", target.mapper_name);
    let devid = devid_for_mapper_path(&pool, &mapper_path).ok_or_else(|| {
        RecoverError::AckCleanupFailed {
            stage: "live-pool add recovery (target sweep)",
            detail: format!("{name}: not found in live pool"),
        }
    })?;
    sweep_devids.push(devid);
}
alert::drop_ghost_acked_for_devids(params.paths, &sweep_devids).map_err(|e| {
    RecoverError::AckCleanupFailed {
        stage: "live-pool add recovery (target sweep)",
        detail: e.to_string(),
    }
})?;
```

Per-arm cleanup at 6a stays. Together they enforce: every journaled target
gets its ghost dropped before save_membership, regardless of whether it
arrived via replay, initial open/scan reconcile, or "already live at entry."

### 7. RemoveMissing PostMaintenance hygiene -- `cli/src/recover.rs:2371`

Between `journal::clear_journal(params.paths)...?` at line 2427 and the
success `eprintln!("pending-op.json cleared. Recovery complete.")` at
line 2428, insert:

```rust
// Hygiene -- mirrors remove_missing.rs:284. devid is the field already
// destructured from RemoveMissingPostCtx at line 2380.
if let Err(e) = alert::drop_ghost_acked_for_devids(params.paths, &[devid]) {
    eprintln!("Warning: failed to update acked stats: {e}");
}
```

### 8. Replace recovery -- NO CHANGE

`execute_replace_pool_mutation_recovery` (`recover.rs:2664`) and
`execute_replace_post_maintenance_recovery` (`recover.rs:2763`) intentionally
leave `acked-stats.json` untouched. Live `replace.rs` does the same because
btrfs replace preserves devid in-place; the existing baseline maps to the
same logical slot. Adding cleanup here would drop a legitimate baseline.

### 9. ADR update -- `docs/decisions/014-alerts.md:126-136`

The current "Acked-stats hygiene across pool membership changes" section
says "Three layers enforce it" and names only `cmd_add`, `cmd_remove`,
`cmd_remove_missing`, and `cmd_monitor`. This change makes recovery a
first-class callsite for every command in layers 1 and 2.

Update the section to name recovery counterparts. Rewrite layers 1 and 2 as:

```markdown
1. **Add-time guard (correctness boundary):** `cmd_add` clears acked-stats
   unconditionally on bootstrap and drops the assigned devid per-disk inside
   the live-pool add loop. `cmd_recover`, when finishing an interrupted add,
   mirrors both: bootstrap-recovery calls `remove_acked_stats`, and live-add
   recovery drops every journaled target's devid (per-arm after a replayed
   `pool_add_device`, and via a final sweep when the target was already live
   at recovery entry -- the committed-but-closed crash window). Cleanup
   failure here is command-fatal in both `cmd_add` and `cmd_recover`: the
   error names the stage and instructs the user to delete the file before
   relying on alerts.
2. **Remove-time prune (hygiene):** `cmd_remove` and `cmd_remove_missing`
   drop the affected devid on success. `cmd_recover` mirrors the prune for
   committed removes only -- the Remove guard at `recover.rs:965-980` may
   restore a target whose eviction did not complete, in which case its
   acked-stats entry is a legitimate baseline that must survive. Cleanup
   failure here is non-fatal (warning) -- the next `add` for that devid will
   catch it via layer 1.
```

Layer 3 (monitor) is unchanged.

Also append one sentence to the `cmd_remove` description note (around line
131 of the ADR, near "Ack state keyed by btrfs devid"): "The `cmd_remove`
planner enriches the journaled `pre_membership` with the target's live
btrfs devid so recovery can resolve it after a discover-time `pool.json`."

## Tests

All tests live inline in `cli/src/recover.rs` test module (line 3227+). The
existing harness uses `PoolFixture` and `RemountHarness` (line 3233). Each
new test gets the AGENTS.md `// Intent / Why it exists / Scenario` three-section
preamble per `AGENTS.md` "Test Conventions".

For seeding `acked-stats.json` in tests, reuse the path-and-file pattern from
`alert.rs:1169` (`remove_acked_stats_deletes_file_and_allows_missing`):
build `AckedStats` in-memory, write via `alert::save_acked_stats(&acked, &paths)`.

### Test 1 -- `bootstrap_recovery_clears_acked_stats`

- Intent: bootstrap recovery deletes any pre-existing `acked-stats.json`.
- Why: an interrupted-then-recovered bootstrap that skipped `remove_acked_stats`
  would silently bind old-pool acked baselines to new-pool devids.
- Scenario: user wipes a pool, starts `braid add disk1 disk2`, crashes between
  `pool_bootstrap_mount_raid1` and `remove_acked_stats`. Recovery completes
  the bootstrap. Old `acked-stats.json` with entries for devids 1, 2, 7 must
  be gone.

Drive `cmd_recover` against an `Add{PoolMutation}` journal with empty
`pre_membership` and a two-disk `target_membership`. Pre-seed
`acked-stats.json` with entries for devids 1, 2, 7. Assert
`!f.paths.acked_stats_json().exists()` after recovery.

### Test 2a -- `live_add_recovery_drops_ghost_for_reused_devid_via_replay`

- Intent: live-add recovery drops the assigned devid's ghost entry inside
  the replay loop, before moving to the next target.
- Why: btrfs reuses the max removed devid on next add; if the previous
  holder's acked baseline survives a recovered partial multi-add, the new
  disk inherits suppressed alerts.
- Scenario: pool was `{1,2,3,4}`, devid 4 removed cleanly, `acked-stats.json`
  retained a stale entry for devid 4. User runs `braid add disk_new`,
  btrfs has NOT yet completed `pool_add_device` at crash time. Recovery
  replays the add via `pool_add_device` and must drop devid 4.

Build the existing `recover_add_pool_mutation_freshluks_*` pattern around
line 5800 in the test module. The journal's `pre_membership` is non-empty
and the post-crash pool does NOT contain the target (so the replay loop
fires). Seed `acked-stats.json` with `{"4": {missing_acked: false,
read_io_errs: 5}}` and a control entry for devid 1 (must remain byte-equal).
Drive recovery. Assert `BtrfsDeviceAdd` was recorded by the runner. Assert
devid 4 absent, devid 1 byte-equal.

### Test 2b -- `live_add_recovery_drops_ghost_for_committed_but_closed_target`

- Intent: live-add recovery sweeps ghosts for targets that were already
  pool_add_device'd in btrfs at recovery entry (the replay loop is skipped
  entirely via the outer `add_targets_all_live` gate).
- Why: per-arm cleanup runs only inside the replay loop; without the
  pre-save_membership sweep, a crash AFTER `pool_add_device` succeeded but
  BEFORE the live ghost-drop ran leaves the stale entry intact.
- Scenario: same {1,2,3,4} -> remove devid 4 -> add disk_new scenario as 2a,
  but the original `add` crashed AFTER `pool_add_device` succeeded for
  disk_new (devid 4 already live in btrfs). Recovery enters with
  `add_targets_all_live(&pool, targets) == true` and skips the replay loop.

Configure the post-crash `probe_pool` mock so disk_new is already live at
devid 4. Seed `acked-stats.json` with `{"4": {read_io_errs: 5}}` and a
devid-1 control. Drive recovery. Assert NO `BtrfsDeviceAdd` was recorded
(replay loop skipped). Assert devid 4 absent, devid 1 byte-equal. This
test fails without the all-live sweep callsite in Change 6b.

### Test 2c -- `live_add_recovery_drops_ghosts_for_mixed_batch`

- Intent: live-add recovery sweeps ghosts for targets skipped via the
  per-iteration `continue` at `recover.rs:2158`, even when other targets
  in the same batch are replayed.
- Why: the inner skip path (target already in `live_member_names(&pool)`
  at start of iteration) bypasses per-arm cleanup; without the sweep at
  the post-replay save_membership site, partial multi-add recovery returns
  success with a stale baseline for the already-live target.
- Scenario: two-disk add `braid add disk_A disk_B`. disk_A's
  `pool_add_device` completed in btrfs (devid 4 reused from a prior
  remove). disk_B's `pool_add_device` had NOT yet run. Crash. Recovery
  finds disk_A live (skipped via `continue`) and replays disk_B. The
  replay loop fires (outer gate false) but per-arm cleanup runs only for
  disk_B.

Build the existing replay-loop test pattern. Configure the post-crash
`probe_pool` mock so disk_A is live at devid 4 and disk_B is missing; the
post-replay probe puts disk_B at devid 5. Seed `acked-stats.json` with
stale entries for BOTH devid 4 (disk_A's ghost) and devid 5 (disk_B's
ghost), plus a devid-1 control. Drive recovery. Assert one `BtrfsDeviceAdd`
recorded for disk_B's mapper path. Assert devid 4 absent (sweep cleaned it),
devid 5 absent (per-arm cleaned it), devid 1 byte-equal. This test fails
without the post-replay sweep callsite in Change 6b.

### Test 3a -- `remove_recovery_drops_target_devid_when_eviction_committed`

- Intent: GenericLivePool recovery of a committed Remove drops the removed
  devid's acked entry.
- Why: live `remove.rs:356` does this; recovery for the same op kind must
  match or the next add that reuses this devid sees a ghost.
- Scenario: pool `{1,2,3}`, user runs `braid remove diskB` (devid 2), btrfs
  eviction commits, crash before `clear_journal`. Recovery completes the
  bookkeeping and must drop devid 2's acked entry.

Seed `acked-stats.json` with entries for devids 1 and 2. Build a journal
`OpKind::Remove { name: "diskB" }` with `pre_membership` carrying
`diskB.devid = Some(2)` (enriched via Change 3's path). The post-crash
`pool` does NOT contain diskB. Drive `cmd_recover` through GenericLivePool.
Assert `recovered.disks` does not contain "diskB". Assert devid 2 entry
absent, devid 1 byte-equal.

### Test 3b -- `remove_recovery_preserves_target_devid_when_eviction_uncommitted`

- Intent: GenericLivePool recovery of an uncommitted Remove (target still
  in pool via live, null-underlying, or MISSING) preserves the target's
  acked-stats entry.
- Why: `execute_generic_live_pool_recovery` restores not-yet-evicted
  targets at lines 965-980; an unconditional ghost-drop would erase a
  legitimate baseline.
- Scenario: pool `{1,2,3}`, user runs `braid remove diskB` (devid 2),
  eviction fails before commit (e.g. hot-unplug -> null-underlying), crash
  preserves the journal. Recovery finds diskB still present in
  `null_underlying`; recovered membership re-includes diskB; cleanup must
  skip.

Build the same journal as Test 3a but configure `probe_pool` to report
diskB in `null_underlying`. Seed acked-stats with devid 2 entry that must
SURVIVE. Drive recovery. Assert `recovered.disks` contains "diskB". Assert
devid 2 entry byte-equal to seed.

### Test 3c -- `remove_recovery_with_no_devid_journal_skips_cleanup_with_warning`

- Intent: legacy journals (or no-devid pool.json sources) that pre-date
  Change 3's enrichment fall through to no-op cleanup, not a crash.
- Why: Change 3 enriches future journals; recovery must tolerate journals
  written before the enrichment (or by external tooling) without panicking.
- Scenario: a journal `OpKind::Remove { name: "diskB" }` whose
  `pre_membership.disks["diskB"].devid` is `None`. Recovery finds the
  committed remove state (diskB absent from pool) but cannot resolve the
  devid; cleanup must silently skip.

Build the same shape as Test 3a but with `pre_membership` carrying
`diskB.devid = None`. Drive recovery. Assert recovery succeeds (no panic,
no error). Assert acked-stats unchanged. The defense-in-depth monitor
reconcile will eventually drop the orphan on a future cycle.

### Test 3d -- `remove_recovery_warning_only_on_corrupt_acked_stats`

- Intent: when `drop_ghost_acked_for_devids` fails inside committed Remove
  recovery (e.g. corrupt `acked-stats.json`), recovery still succeeds and
  the journal is cleared. The corrupt bytes are left intact for the next
  monitor cycle to handle.
- Why: the hygiene policy classifies Remove cleanup failure as non-fatal
  warning (mirrors `remove.rs:356-359`). Without this test, a future
  implementation could propagate the error via `?` and leave the journal
  in place, turning a hygiene miss into a stuck recovery.
- Scenario: committed Remove scenario as Test 3a, but `acked-stats.json`
  is seeded with non-JSON bytes. Recovery must complete the bookkeeping
  without rewriting the file; the next `cmd_monitor` cycle will see the
  unreadable file, latch `ComputationError` via
  `latch_computation_error("acked-stats unreadable -- {e}", ...)`
  (`monitor.rs:97-101`), and leave the corrupt bytes in place for
  forensics.

Build Test 3a's journal + `recovered.disks` shape. Seed `acked-stats.json`
with non-JSON bytes (e.g. `b"corrupt"`). Drive recovery. Assert
`result.is_ok()`. Assert `!f.paths.pending_op_json().exists()` (journal
cleared). Assert `std::fs::read(&f.paths.acked_stats_json()).unwrap() ==
b"corrupt"` (the failed `drop_ghost_acked_for_devids` aborted on the
fallible-loader parse error before reaching `save_acked_stats`, so the
file bytes are byte-equal to the seed). Do NOT assert on stderr text:
the `eprintln!("Warning: ...")` warning bypasses
`status_tag::testing::capture_with`, which only captures `emit_status`
output -- a stderr-text assertion would be unimplementable.

### Test 4a -- `remove_missing_post_maintenance_recovery_drops_devid`

- Intent: RemoveMissing PostMaintenance recovery drops the removed devid's
  acked entry.
- Why: live `remove_missing.rs:284` does this; recovery must match.
- Scenario: interrupted `braid remove-missing` resumed at PostMaintenance.

Extend the existing PostMaintenance test family (search for
`remove_missing_post_maintenance` in the test module). Seed entry for the
removed devid. Drive recovery. Assert entry absent post-call.

### Test 4b -- `remove_missing_post_maintenance_recovery_warning_only_on_corrupt_acked_stats`

- Intent: when `drop_ghost_acked_for_devids` fails inside RemoveMissing
  PostMaintenance recovery, recovery still succeeds and the journal is
  cleared. The corrupt bytes are left intact.
- Why: same hygiene policy as Test 3d (mirrors `remove_missing.rs:284`).
  Without this test, a future implementation could turn the warning into
  an `?`-propagated error and strand recovery.
- Scenario: Test 4a's journal shape, but `acked-stats.json` is seeded
  with non-JSON bytes.

Build Test 4a's setup. Seed `acked-stats.json` with non-JSON bytes. Drive
recovery. Assert `result.is_ok()`. Assert `!f.paths.pending_op_json().exists()`.
Assert `std::fs::read(&f.paths.acked_stats_json()).unwrap() == b"corrupt"`
(same fallible-loader-aborts-before-save reasoning as Test 3d). Do NOT
assert on stderr text -- the `eprintln!("Warning: ...")` warning is not
captured by `status_tag::testing::capture_with`.

### Test 5a -- `bootstrap_recovery_ack_cleanup_failure_returns_typed_error_and_preserves_journal`

- Intent: when `remove_acked_stats` fails, recovery returns
  `RecoverError::AckCleanupFailed` and the journal survives.
- Why: pins the fail-closed contract -- the next `braid recover` retries
  cleanup instead of users discovering stale alerts silently.
- Scenario: filesystem error during the bootstrap recovery cleanup write.

Inject an I/O failure on `acked-stats.json` removal (e.g. point the file's
parent at a read-only directory, or pre-create the path as a non-empty
directory so `remove_file` returns `Errno::EISDIR`). Drive recovery. Assert
`matches!(err, RecoverError::AckCleanupFailed { stage: "bootstrap-recovery", .. })`
and `f.paths.pending_op_json().exists()` (journal preserved). Pattern matches
existing `inhibitor_failure_preserves_journal`-shape tests.

### Test 5b -- `live_add_recovery_ack_cleanup_failure_returns_typed_error_and_preserves_journal`

- Intent: when `drop_ghost_acked_for_devids` fails inside live-add recovery
  (per-arm OR sweep callsite), recovery returns
  `RecoverError::AckCleanupFailed` with stage matching the live-pool add
  recovery context, and the journal survives.
- Why: without this test a future implementation could silently swallow
  read/parse errors on `acked-stats.json` during live-add recovery and
  clear the journal anyway, defeating the fail-closed boundary that Tests
  2a-2c only exercise on the success path.
- Scenario: the original add interrupted mid-replay; an old, corrupt
  `acked-stats.json` (non-JSON bytes) was on disk at recovery time. The
  fallible loader `load_acked_stats_fallible` inside
  `drop_ghost_acked_for_devids` propagates the parse error; recovery must
  abort with the typed variant and leave the journal in place.

Build the Test 2a journal shape (single replayed target). Seed
`acked-stats.json` with non-JSON bytes (e.g. `"corrupt"`). Drive recovery.
Assert `matches!(err, RecoverError::AckCleanupFailed { stage, .. })` where
`stage` starts with `"live-pool add recovery"` (covers both per-arm and
sweep variants). Assert `f.paths.pending_op_json().exists()` (journal
preserved). Pattern matches Test 5a's shape.

### Test 6 -- `remove_journal_pre_membership_carries_target_devid`

- Intent: `cmd_remove` writes the target's live btrfs devid into the
  journal's `pre_membership` before mutating the pool.
- Why: recovery's hygiene cleanup uses this devid; without enrichment the
  cleanup silently skips for any `pool.json` written via
  `DiskMember::from_by_id` (discover, initial add). The journal is the
  only artifact recovery can read, so the enrichment must be observable
  through `journal::load_journal` after a real `cmd_remove` invocation.
- Scenario: starting `pool.json` from `two_disk_healthy()`
  (`test_fixtures/shared.rs:203`) -- members `disk1` and `disk2`, both
  with `devid: None`. The live pool reports `disk2` at devid 2. Run
  `cmd_remove disk2` against a mock runner that returns non-zero exit on
  `BtrfsDeviceRemove` so the eviction fails AFTER `journal::write_journal`
  but BEFORE `journal::clear_journal`. The journal stays on disk for
  inspection per the existing "preserved for recover" semantics
  (`remove.rs:314-315`).

Model on the existing post-journal eviction-failure test at
`remove.rs:1055-1083`. After `cmd_remove` returns Err, load the journal
via `journal::load_journal(&f.paths).unwrap().unwrap()` and assert
`journal.pre_membership.disks["disk2"].devid == Some(2)`. Also assert
`journal.target_membership.disks.contains_key("disk2") == false` (the
eviction transformation is intact). Lives in `cli/src/remove.rs` test
module (not recover), because the change under test is in `cmd_remove`
itself.

## Critical files

- `cli/src/recover.rs` -- six callsite edits (one new error variant; bootstrap
  cleanup; commit-gated Remove cleanup; per-arm live-add cleanup; all-live
  sweep cleanup; RemoveMissing PostMaintenance hygiene) + tests.
- `cli/src/add.rs` -- visibility change + doc comment on
  `devid_for_mapper_path` at line 639.
- `cli/src/remove.rs` -- enrich journal `pre_membership` with
  `work_plan.target_devid` between lines 296-297 + new enrichment test.
- `docs/decisions/014-alerts.md` -- rewrite layers 1 and 2 (lines 132-135)
  to name recovery counterparts; append one sentence about the Remove
  journal enrichment.
- `cli/src/alert.rs` -- no change (helpers already exposed).

## Verification

1. `just test-rust` -- runs the thirteen new inline tests (1, 2a, 2b, 2c,
   3a, 3b, 3c, 3d, 4a, 4b, 5a, 5b, 6) plus the existing `alert::tests`,
   `add::tests`, `remove::tests`, `remove_missing::tests`,
   `recover::tests` suites. Expected: all pass.
2. `just test-vm` -- existing recover VM checks (e.g. tests under
   `tests/cli/braid-recover*.py`) cover the live-pool reconstruction; this
   change does not break them. No new VM check is added -- the seven inline
   tests cover the gap end-to-end at the unit-test layer.
3. Skim the rewritten `docs/decisions/014-alerts.md` "Acked-stats hygiene"
   section against the seven recovery branches in the Approach table to
   confirm every branch is named.
