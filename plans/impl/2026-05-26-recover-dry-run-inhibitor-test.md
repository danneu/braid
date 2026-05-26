# Fix: make the recover dry-run inhibitor test exercise `cmd_recover`

## Context

`recover_dry_run_does_not_acquire_sleep_inhibitor` (`cli/src/recover.rs:11275-11300`)
calls `plan_recover` and asserts `f.inhibitor.acquire_count() == 0`. But the sleep
inhibitor is acquired *only* inside the `execute_*`/`finish_*` helpers, never in
`plan_recover` -- the planner has no acquire site at all. So the assertion is true by
construction: it would still pass even if a regression deleted the dry-run short-circuit
and made `cmd_recover` fall through to `plan.execute()`.

The behavioral guarantee the test name claims -- "dry-run does not acquire" -- is enforced
solely by this branch in `cmd_recover` (`recover.rs:1549-1552`):

```rust
if params.dry_run {
    plan.preview().print_colored();
    return Ok(());
}
plan.execute(runner, fs, by_id_resolver, params)
```

No test in the file exercises that branch: all nine `dry_run(true)` tests call
`plan_recover`, none call `cmd_recover`. This change retargets the test to drive
`cmd_recover` with a fixture whose execute path *does* acquire, so the assertion pins the
short-circuit instead of a trivially-true planner property.

### Why the fixture choice is load-bearing (not just an example)

The current test's fixture (`recoverable_pool_mutation_add_journal()`, pool not mounted)
will NOT work for this. In `execute_add_pool_mutation_recovery` the acquire
(`recover.rs:2389`) sits *after* runner-command-dependent work (`ensure_luks_open`,
`scan_mapper_if_btrfs_visible`, `probe_pool`, `verify_recover_passphrase_for_add_replay`).
If the dry-run branch were removed, execute would error on a missing mock *before*
reaching the acquire -- `acquire_count()` stays 0 and the test stays green. Hollow again.

A **committed replace post-maintenance** journal is the right fixture:
`execute_replace_post_maintenance_recovery` (`recover.rs:3153`) reaches its acquire
(`recover.rs:3212`, gated only on `!inhibitor_already_held`) after just filesystem/resolver
steps (`live_pool_matches_membership`, `load_membership`,
`recover_membership_matching_expected`, `save_membership`) -- no runner command in between.
The standalone PostReplaceMaintenance dispatch passes `inhibitor_already_held: false`, so the
acquire fires. With the dry-run branch removed, execute reaches the acquire and
`acquire_count()` becomes 1 -- regression caught.

## Change: rewrite the test in place at `recover.rs:11275-11300`

Model it directly on the proven, already-passing full-command test
`cmd_recover_replace_post_maintenance_preserves_non_target_missing_disk`
(`recover.rs:10813-10926`). That test drives `cmd_recover` end-to-end through
`execute_replace_post_maintenance_recovery` to completion (writes pool.json, resizes,
clears the journal). It just uses the noop inhibitor and never checks `acquire_count`.
Because it is proven to drive execute *past* the acquire, reusing its fixture/mocks is what
guarantees the dry-run variant is a real regression catcher.

Steps:

1. Copy the fixture, journal, `MockRunner`/`MockFs` mock setup, and `resolver_for(...)`
   from `recover.rs:10814-10905` verbatim (the journal is `OpKind::Replace { phase:
   PostReplaceMaintenance, source: Missing { old_devid: 2 }, restore_raid1_after_commit:
   false, .. }` on a mounted 3-device pool). Keep the `BtrfsFilesystemResize` mock; it goes
   unconsumed in dry-run and is harmless, and keeping the setup identical to the proven
   model minimizes risk.
2. Build params with **both** `.dry_run(true)` and `.sleep_inhibitor(&f.inhibitor)`:
   ```rust
   let params = f
       .recover_params()
       .passphrase_file(None)
       .dry_run(true)
       .sleep_inhibitor(&f.inhibitor)
       .build();
   ```
3. Call the public command (note the `&resolver` arg that `plan_recover` does not take):
   ```rust
   cmd_recover(&runner, &fs, &resolver, &params)
       .expect("dry-run recover should preview and return without executing");
   ```
4. Assert the dry-run contract -- primary signal first, then reinforcing no-mutation
   checks that all flip in the regression scenario (execute runs):
   ```rust
   assert_eq!(f.inhibitor.acquire_count(), 0, "dry-run must not acquire the inhibitor");
   assert!(f.paths.pending_op_json().exists(), "dry-run must not clear the journal");
   assert!(
       membership::load_membership(&f.paths).is_err(),
       "dry-run must not write pool.json"
   );
   assert!(
       !runner.requests().iter().any(|r|
           matches!(r, CmdRequest::BtrfsFilesystemResize { .. })),
       "dry-run must not issue the post-replace resize"
   );
   ```
   (Confirm the exact "no pool.json" predicate against `membership::load_membership` on an
   empty `PoolFixture` -- it may return `Ok(empty)` rather than `Err`; if so assert the
   loaded membership is empty / has no members instead. Mirror whatever
   `recover.rs:10910` does, inverted.)

All symbols above (`uuid_for_name`, `membership_from`, `membership_entry`,
`disk_member_named`, `mountpoint_ok`, `cryptsetup_status_active`, `cryptsetup_uuid_ok`,
`ok_raw`, `ok_raw_empty`, `resolver_for`, `CmdRequest`, `MapperName`, `MountPoint`) are
already in scope in the same `mod tests` -- no new imports.

### Add the required test preamble

The current test has no preamble; CLAUDE.md Test Conventions require Intent / Why it exists
/ Scenario. Add (adjust wording to match the file's `//` style):

- **Intent:** `cmd_recover` with `dry_run = true` previews and returns via the
  short-circuit without acquiring the sleep inhibitor or performing any execute-phase
  mutation, even for a journal whose real-run execute path *does* acquire.
- **Why it exists:** the guarantee is enforced only by the
  `if params.dry_run { plan.preview(); return Ok(()) }` branch in `cmd_recover`. A
  regression deleting it would fall through to `plan.execute()` and acquire. The prior
  version drove `plan_recover`, which has no acquire site, so it asserted a
  construction-true property and could not catch that regression.
- **Scenario:** an interrupted `braid replace` committed the pool mutation
  (PostReplaceMaintenance) so recovery would resize the new devid and clear the journal;
  the operator runs the recovery with `--dry-run` first to preview and must observe zero
  inhibitor acquisitions and zero state mutation.

### Placement

Rewrite in place at `recover.rs:11275` (preserves the name and git-blame continuity of "the
dry-run inhibitor test"). Relocating it next to its non-dry-run sibling at
`recover.rs:10926` is an acceptable alternative if reviewer prefers locality, but in-place
is the recommendation -- single focused edit.

## Files

- `cli/src/recover.rs` -- rewrite the one test at `:11275-11300`. No production code changes.

## Verification

1. **Negative control (proves the test is not hollow).** Temporarily comment out the
   `if params.dry_run { ... return Ok(()) }` block at `recover.rs:1549-1552`, run the test,
   and confirm it now **fails** on `acquire_count() == 0` (becomes 1) and the no-mutation
   asserts. Restore the block. This is the crux: the old test could not fail under this
   regression; the new one must.
2. `just test-rust` -- run the full Rust unit suite (the CLI crate is `braid-cli`; prefer
   the recipe). Confirm the rewritten test passes with the short-circuit in place.
   Optionally scope first: `cargo test -p braid-cli recover_dry_run_does_not_acquire_sleep_inhibitor`.
3. No VM tests or fixture refresh needed -- this is a Rust unit test change with no parser
   or systemd surface.
