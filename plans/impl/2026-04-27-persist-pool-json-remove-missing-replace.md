# Persist `pool.json` before post-mutation maintenance for `remove-missing` and `replace`

## Context

Commit `de1342c` ("fix(cli): persist add membership before post-add balance")
narrowed the documented invariant for `pool.json`:

- `pool.json` reflects committed btrfs membership.
- `pending-op.json` covers the full mutation lifecycle, including post-mutation
  maintenance (soft balance, resize).
- Maintenance can still be owed while the journal exists; `braid recover`
  replays it.

`braid add` was updated to match this invariant. `braid remove-missing` and
`braid replace` still write `pool.json` only after all post-mutation
maintenance has finished. An interrupted post-mutation phase therefore leaves
`pool.json` stale -- disagreeing with the live pool that already accepted the
membership change -- forcing the operator into recovery just to reconcile
bookkeeping.

This plan brings `remove-missing` and `replace` in line with the refined
invariant. `braid remove` (live) and `braid add` are out of scope: live
`remove`'s long phase IS the membership commit, and `add` was already fixed.

## Code changes

### 1. `cli/src/remove_missing.rs`

Today, `cli/src/remove_missing.rs:191-213` runs:

```
pool_remove_device_using(...)?;       // membership commit
maybe_restore_raid1(...)?;            // post-mutation maintenance
membership::save_membership(...)?;    // pool.json write (TOO LATE)
journal::clear_journal(...)?;
```

Reorder to:

```
pool_remove_device_using(...)?;                              // membership commit
membership::save_membership(&target_membership, params.paths)
    .map_err(|e| RemoveMissingError::Validation(
        format!("failed to persist pool membership: {e}")))?; // pool.json write
maybe_restore_raid1(...)?;                                    // post-mutation maintenance
journal::clear_journal(...)?;
```

No enrichment is needed: `target_membership` (built at line 174-182) is
`pre_membership` minus the removed missing entry; the remaining entries
already carry their `luks_uuid`/`devid`/`added_at`.

Replace the existing block comment ("Post-commit: write pool.json and clear
journal only after the full operation succeeds.") with two narrower comments:

- Above `save_membership`: `// Membership committed by btrfs device remove.`
  `// Persist before the post-remove soft balance; the journal still covers maintenance.`
- Above `clear_journal`: `// Maintenance complete -- safe to clear the journal.`

### 2. `cli/src/replace.rs`

Today, `cli/src/replace.rs:360-426` runs:

```
pool_replace_device(...)?;            // membership commit
[best-effort old mapper close]        // live-only cleanup
pool_resize_device(...)?;             // post-mutation maintenance
maybe_restore_raid1(...)?;            // post-mutation maintenance (missing path)
[enrich target_membership inline from probe_pool]
membership::save_membership(...)?;    // pool.json write (TOO LATE)
journal::clear_journal(...)?;
```

Reorder to:

```
pool_replace_device(...)?;                              // membership commit
let mut target_membership = target_membership;
if let Ok(pool_after) = probe_pool(runner, config.mount_point()) {
    membership::enrich_from_pool_state(                 // pure helper, see (3)
        &pool_after, &mut target_membership);
}
membership::save_membership(&target_membership, params.paths)
    .map_err(|e| ReplaceError::Validation(
        format!("failed to persist pool membership: {e}")))?; // pool.json write
[best-effort old mapper close]                          // live-only cleanup
pool_resize_device(...)?;                               // post-mutation maintenance
[maybe_restore_raid1, missing path]?;                   // post-mutation maintenance
journal::clear_journal(...)?;
```

Mapper close placement: keep it after `save_membership` and before
`pool_resize_device`, preserving today's "close before resize so a resize
`?` does not strand the dm slot" rationale (see
`cli/src/replace.rs:2317-2330` and existing test
`close_runs_before_resize_on_live_replace`). The close has nothing to do
with btrfs membership, so running it after the membership write reads
correctly.

Note on a rare regression: if `save_membership` errors (disk full on
`/var/lib/braid`), the close + resize + balance do not run. Today they all
run before save, so close happens even on later failures. The new ordering
trades a one-disk-full edge case (mapper stays open until `braid lock` /
reboot; recovery rebuilds pool.json from probe but does not close mappers)
for a much more common safety win (interrupted post-mutation maintenance no
longer leaves pool.json stale). Acceptable.

### 3. Shared enrichment helper -- pure, in `cli/src/membership.rs`

`cli/src/add.rs:286-307` already has `fn enrich_from_live_pool(runner,
mount_point, &mut PoolMembership)` that probes inside the helper. The
inline block at `cli/src/replace.rs:407-421` is the same logic. Two callers
crosses the bar to extract, but the helper must stay pure so
`membership.rs` does not gain a `CommandRunner` / `probe_pool` dependency
(it already only knows `PoolState` via `refresh_pool_metadata`).

Add to `cli/src/membership.rs`:

```rust
/// Enrich an in-memory PoolMembership with luks_uuid, devid, and added_at
/// drawn from a freshly probed PoolState. Callers own the probe so this
/// module stays free of CommandRunner / probe coupling.
pub(crate) fn enrich_from_pool_state(
    pool: &PoolState,
    membership: &mut PoolMembership,
) {
    for dev in &pool.devices {
        let Some(name) = crate::config::name_from_mapper(&dev.mapper.0) else {
            continue;
        };
        if let Some(member) = membership.disks.get_mut(name) {
            member.luks_uuid = Some(dev.luks_uuid.clone());
            member.devid = Some(dev.devid);
            if member.added_at.is_none() {
                member.added_at = Some(crate::util::now_iso());
            }
        }
    }
}
```

Then in `add.rs`, delete the local `enrich_from_live_pool` and rewrite both
existing call sites to:

```rust
if let Ok(pool_after) = probe_pool(runner, mount_point) {
    membership::enrich_from_pool_state(&pool_after, &mut final_membership);
}
```

In `replace.rs`, replace the inline enrichment block (lines 407-421) with
the same pattern. `remove_missing.rs` does not need it.

This keeps the "best-effort silent on probe failure" semantics unchanged
because each call site wraps the probe in `if let Ok(...)`.

## Tests -- failure-injection at the seam, not VM observation

VM-level tests are not the right level for this invariant:

- The missing-path replace shape that would naturally observe the
  post-replace soft balance is documented as infeasible end-to-end:
  `cli/src/replace.rs:3169` notes that the degraded single-profile writes
  needed to motivate the soft balance also make `btrfs replace start`
  fail with kernel ENOSPC.
- For remove-missing, "observe a running balance and assert pool.json"
  introduces timing fragility for an invariant that is really about the
  ordering between two adjacent statements in `cmd_remove_missing` /
  `cmd_replace`.

The right shape is a command-layer regression: force the next maintenance
step to fail after the membership op succeeds, and assert that
`pool.json` already reflects the new membership while
`pending-op.json` still exists. Existing peers in the same test modules
already use this pattern.

### remove-missing: extend `journal_survives_soft_balance_failure`

`cli/src/remove_missing.rs:1683-1731` already builds a 3-device pool,
mocks `btrfs device remove` success, and uses `FailingSoftBalanceRunner`
to fail the post-removal soft balance. It currently asserts the journal
survives. Extend with two assertions immediately after the existing
`journal::load_journal(...).is_some()` check:

```rust
let saved = membership::load_membership(&state_paths)
    .expect("pool.json must exist after the membership commit");
assert!(
    !saved.disks.contains_key(<missing-disk-name>),
    "pool.json must reflect the removed missing disk even when the \
     post-remove soft balance fails (saved: {:?})",
    saved.disks.keys().collect::<Vec<_>>()
);
assert!(
    saved.disks.contains_key(<surviving-disk-name>),
    "surviving disks must remain in pool.json"
);
```

(Use the names from `three_device_config` at the top of the same
`mod tests`.)

This pins the invariant: revert the `save_membership` call back to its
old position and the test fails because the FailingSoftBalanceRunner
returns before save runs.

### remove-missing: update comment on `journal_survives_device_remove_failure`

`cli/src/remove_missing.rs:1609-1622` carries the old invariant in its
"Why it exists" preamble: "remove-missing must not persist the target
pool.json until the full mutation succeeds." Under the new ordering
that wording is wrong; the actual guarded boundary is narrower. The
test's behavior remains correct (btrfs device remove fails, so
`save_membership` is never reached, so `pool.json` keeps `disk3` --
assertions at lines 1654-1660 still hold).

Rewrite the "Why it exists" sentence to name the narrower invariant,
e.g.:

```text
Why it exists: remove-missing must not persist the target pool.json
until `btrfs device remove` succeeds. If save_membership ran before
pool_remove_device_using, this device-remove failure would leave
pool.json reconciled without the btrfs operation having committed.
```

No assertion changes; comment-only.

### replace (live arm): extend `close_runs_before_resize_on_live_replace`

`cli/src/replace.rs:2317-2419` already exercises the case where
`btrfs filesystem resize` fails after a successful `btrfs replace start`.
It uses `ResizeFailingLoggingRunner` and asserts the close ran before the
resize call and the journal survives.

Two changes:

1. **Update the runner** at
   `cli/src/replace.rs:2237-2306` so its `BtrfsFilesystemShow` reflects
   the post-replace topology after `BtrfsReplaceStart` succeeds. Today
   it always returns `disk1 + disk2`; with the new early save,
   `replace.rs` calls `probe_pool` before `save_membership` and the
   probe must see `disk1 + disk3` so `enrich_from_pool_state` populates
   `disk3` (which IS in `target_membership`) rather than no-oping
   against the stale `disk2` (which is NOT in `target_membership`).

   Mirror the pattern used by `MissingPathSuccessRunner`
   (`cli/src/replace.rs:3162-3415`): add a `replace_done:
   Arc<AtomicBool>` field, set it to true on the
   `CmdRequest::BtrfsReplaceStart` arm, and gate the
   `BtrfsFilesystemShow` arm to return the disk1+disk3 fixture when
   `replace_done.load(Relaxed)` is true (and the original disk1+disk2
   fixture beforehand). Keep `cryptsetup` `luksUuid` / `status` /
   `luksDump` arms returning the disk3 mappings already present in the
   runner so enrichment has fields to copy.

2. **Add assertions** after the existing journal check:

   ```rust
   let saved = membership::load_membership(&paths)
       .expect("pool.json must exist after the membership commit");
   assert!(
       !saved.disks.contains_key("disk2"),
       "old disk must be gone from pool.json once btrfs replace succeeds, \
        even when the post-replace resize fails"
   );
   assert!(
       saved.disks.contains_key("disk3"),
       "new disk must be in pool.json once btrfs replace succeeds"
   );
   let disk3 = saved.disks.get("disk3").unwrap();
   assert!(
       disk3.luks_uuid.is_some() && disk3.devid.is_some()
           && disk3.added_at.is_some(),
       "new disk must carry enriched metadata: {disk3:?}"
   );
   ```

This pins both the ordering and the enrichment: revert the early save
and the resize-fail test sees the old/new disks in their pre-replace
positions.

### replace (missing arm): new sibling test of the existing soft-balance wiring test

`cli/src/replace.rs:3162-3415` (`cmd_replace_missing_path_runs_soft_balance_after_replace_and_resize`)
already builds the missing-path scaffolding: a 2-disk membership where
disk2 is missing (devid 2), a `MissingPathSuccessRunner` that flips the
btrfs probe from degraded to healthy after `BtrfsReplaceStart`, and a
`PresentLuks { mapper_open: true }` `ReplaceMockFs` so cmd_replace skips
LUKS init.

Add a sibling test in the same `mod tests` named
`pool_json_persisted_when_missing_path_soft_balance_fails`. Reuse the
same setup but swap the runner for one that succeeds through replace +
resize and fails on `BtrfsBalanceRaid1Soft`. Assertions:

- Result is `Err(...)` (specifically `ReplaceError::Pool(...)`).
- `pool.json` no longer contains `disk2`, contains `disk3` with enriched
  `luks_uuid`/`devid`/`added_at`.
- `pending-op.json` still exists.

This is the only NEW test required. Follow
`feedback_local_runner_over_shared_mock`: write a purpose-built file-local
runner (e.g. `MissingPathBalanceFailingRunner`) rather than widening
`MockRunner`.

### Test file conventions

Per `feedback_test_preamble_block_comment_literal`, the new test must
begin with a literal `/* ... */` block comment with `Intent` /
`Why it exists` / `Scenario`. The two extensions to existing `// ... //`
comment-style tests can keep their existing comment style (this is
in-place editing, not a new test file).

### What is intentionally NOT added

- No new VM tests. The existing `tests/module/ups-lb-during-remove-missing.py`
  and `tests/module/ups-lb-during-replace.py` already cover the recovery
  path after a forced shutdown during the post-mutation soft balance, and
  remain valid: the new code merely shifts when `pool.json` is written;
  recovery's flow (probe live pool -> save_membership ->
  replay_post_mutation -> clear_journal) is idempotent against an
  already-current `pool.json`.
- No flake.nix changes (no new VM test to register).

## Recovery (`cli/src/recover.rs`) -- no code change

`cmd_recover` does: probe live pool -> save_membership(recovered) ->
replay_post_mutation -> clear_journal (`cli/src/recover.rs:391-401`).
After this fix, a crash between the early `save_membership` and the
post-mutation maintenance is handled the same way it always was: recovery
overwrites `pool.json` with its own probe-derived value (idempotent),
runs replay (resize for replace, soft balance for replace/remove-missing,
both already idempotent under `,soft` and `max`), then clears the
journal. Existing `cli/src/recover.rs` tests do not need updating.

## Docs

`docs/principles.md` already states the refined invariant generically
and needs no further update.

`docs/decisions/017-runtime-disk-membership.md`'s "Mutation ordering"
section currently gives examples for `add` and `remove`. Append two more
example sentences in the same paragraph:

> For `remove-missing`, membership commits when `btrfs device remove
> <devid>` against the missing devid returns success; the post-remove
> soft balance that restores RAID1 redundancy for chunks created during
> degraded operation is follow-up maintenance.
>
> For `replace`, membership commits when `btrfs replace start -B`
> completes; the post-replace resize, and (for missing-path
> replacements that clear the last missing device) the soft balance,
> are follow-up maintenance.

## Verification

```
cargo test -p braid-cli journal_survives_soft_balance_failure
cargo test -p braid-cli close_runs_before_resize_on_live_replace
cargo test -p braid-cli pool_json_persisted_when_missing_path_soft_balance_fails
cargo test -p braid-cli                # full cli unit + integration
just test-rust
just test-vm ups-lb-during-remove-missing
just test-vm ups-lb-during-replace
just test-vm braid-add-persists-before-balance   # sanity: shared helper change
```

After each new/extended test passes, sanity-check the gate: temporarily
revert just the `save_membership` reorder in the corresponding command
and confirm the assertions fail for the right reason
(`feedback_test_at_failure_layer`). Restore the fix.

Per `feedback_dont_run_just_test_all_autonomously`, do not run
`just test-all` autonomously after the targeted runs pass; let the user
drive the full-suite re-run.

## Critical files

- `cli/src/remove_missing.rs` (reorder save vs maintenance; extend
  `journal_survives_soft_balance_failure` with pool.json assertions;
  rewrite the "Why it exists" comment on
  `journal_survives_device_remove_failure` to name the narrower
  invariant)
- `cli/src/replace.rs` (reorder; drop inline enrich block; update
  `ResizeFailingLoggingRunner` to flip its show topology after
  `BtrfsReplaceStart`; extend `close_runs_before_resize_on_live_replace`
  with pool.json assertions; add new sibling test
  `pool_json_persisted_when_missing_path_soft_balance_fails`)
- `cli/src/add.rs` (delete local `enrich_from_live_pool`; switch both
  call sites to `membership::enrich_from_pool_state`)
- `cli/src/membership.rs` (host pure `enrich_from_pool_state(&PoolState,
  &mut PoolMembership)`)
- `docs/decisions/017-runtime-disk-membership.md` (extend Mutation
  ordering example paragraph)

## Out of scope

- `braid remove` (live): the long btrfs operation IS the membership
  commit; saving before it returns would record the disk as gone while
  btrfs still owns it.
- Recovery (`cli/src/recover.rs`): existing flow is already idempotent
  against early `pool.json` save.
- VM tests: the natural end-to-end shapes for these invariants are
  either documented infeasible (replace missing-path,
  `cli/src/replace.rs:3169`) or fragile to timing; failure-injection
  unit tests at the seam are the robust regression.
- Backwards-compat / migration: per project policy.
