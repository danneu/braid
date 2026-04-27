# braid add: persist pool.json after `btrfs device add`, not after the post-add balance

## Context

In `cli/src/add.rs`, when a disk is added to an *existing* pool, the
ordering of state updates is:

1. write journal (`pending-op.json`) with target membership
2. LUKS-format and open the new disk(s)
3. `btrfs device add /dev/mapper/<new> /mnt/storage` -- the **irreversible
   commit point**: the disk is a btrfs member the moment this returns 0
4. `btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt/storage`
   -- long-running (hours), cancellable, redistributes chunks
5. enrich membership via `probe_pool` (luks_uuid, devid)
6. `save_membership` -- writes `pool.json` with the new disk
7. `clear_journal`

Steps 5-6 only run if step 4 finishes. Because `pool.json` is the
authoritative record of pool membership but is written *after* the slow,
interruptible balance, an interrupt between step 3 and step 6 leaves a
provably wrong on-disk state:

- btrfs filesystem: N+1 devices. The data may be mid-conversion (some
  single-profile chunks, some RAID1) or fully RAID1, depending on where
  the balance was interrupted -- either way, the post-add balance is
  still owed and the journal correctly tracks that.
- `pool.json`: still N devices (stale)
- `pending-op.json`: present, marks Add as pending

`braid status` reads the live btrfs pool and reports N+1 disks; an
operator comparing `braid status` against `pool.json` sees an
unexplained contradiction. The longer the user waits before running
`braid recover`, the more confusing the state is.

The balance does not change membership; it only converts single-profile
chunks to RAID1 across the new device set. Membership is *committed* by
`btrfs device add` -- so `pool.json` should be persisted at that
moment, not after a slow follow-up step.

This aligns with the principle in
`docs/decisions/017-runtime-disk-membership.md`: "`pool.json` reflects
completed operations." The completed operation here is the *membership
change*. The post-add balance is post-mutation work tracked by the
journal, not part of the membership commit.

## Fix

Reorder the add-to-existing-pool branch in `cli/src/add.rs` so
`save_membership` is called immediately after the `pool_add_device` loop
finishes successfully, *before* the post-add balance. The journal
remains in place until the balance finishes; `clear_journal` still runs
after the balance.

The bootstrap branch (no existing pool) is unchanged: there, "filesystem
exists" and "membership exists" are coincident, and persisting after
`mkfs.btrfs` + mount is correct.

### Current shape (`cli/src/add.rs:564-612`)

```text
if !self.pool.mounted {
    // bootstrap: mkfs.btrfs (single or RAID1) + mount
} else {
    for mp in &mapper_paths {
        pool_add_device(runner, mp, mount_point)?;       // <-- commit
    }
    if total_after >= 2 {
        pool_balance_raid1(runner, mount_point, ...)?;   // <-- long
    }
}

// post-commit: probe_pool -> enrich `final_membership` -> save -> clear
let mut final_membership = target_membership.clone();
match probe::probe_pool(runner, mount_point) { ... enrich ... }
membership::save_membership(&final_membership, params.paths)?;
journal::clear_journal(params.paths)?;
```

### New shape

```text
if !self.pool.mounted {
    // bootstrap (unchanged)

    // post-commit: enrich + save + clear (existing block stays here)
} else {
    for mp in &mapper_paths {
        pool_add_device(runner, mp, mount_point)?;
    }

    // NEW: enrich + save_membership BEFORE balance.
    // Membership is now durable; the journal still flags this op as
    // pending so `braid recover` knows the balance is owed.
    let mut interim_membership = target_membership.clone();
    enrich_from_live_pool(runner, mount_point, &mut interim_membership);
    membership::save_membership(&interim_membership, params.paths)?;

    if total_after >= 2 {
        pool_balance_raid1(runner, mount_point, ...)?;
    }

    journal::clear_journal(params.paths)?;
}
```

The bootstrap branch keeps its existing "post-commit: probe -> enrich ->
save -> clear" block. The else branch gets its own equivalent block
inlined right after the device-add loop.

### Helper

The probe + enrich pattern is repeated in two places after this change
(bootstrap branch and else branch). Extract a small file-local helper
that is **byte-for-byte equivalent** to the current
`add.rs:592-606` block -- same fields set (`luks_uuid`, `devid`, and
`added_at` when absent), and same silent-on-probe-failure behaviour:

```rust
// in cli/src/add.rs (file-local)
fn enrich_from_live_pool<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    membership: &mut PoolMembership,
) {
    // Silent on probe failure -- matches today's behaviour. luks_uuid /
    // devid / added_at are best-effort metadata; recovery re-derives
    // them from the live pool if missing.
    if let Ok(pool_after) = probe_pool(runner, mount_point) {
        for dev in &pool_after.devices {
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
}
```

Notes:
- **Do not** introduce an `eprintln!("warning: probe failed...")`
  branch. Today's code stays silent; preserve that.
- **Do not** drop `added_at`. ADR 017 lists it as part of the enriched
  metadata, and existing callers / tests may depend on it being present
  in `pool.json` after `braid add` succeeds.
- File-local is the right scope: `recover.rs` has a near-duplicate that
  *builds* membership from scratch rather than enriching an existing
  one -- consolidating the two would be a wider refactor and is not in
  scope for this fix.

### Multi-disk add: partial-failure semantics

The device-add loop calls `pool_add_device` for each path. If disk A
succeeds and disk B fails, the loop returns `Err(...)` and `save` is
skipped. This matches today's behaviour: `pool.json` stays at
pre-membership, `pending-op.json` is still present, and `braid recover`
is responsible for reconciling. The recovery path (`recover.rs:346-393`)
already builds membership from a live `probe_pool`, so it correctly
records the partial state.

The fix does not regress this case. Incremental in-loop saves (per-disk)
are not necessary for this change; mid-loop device-add failure is a
distinct concern with the same recovery answer.

## Files to modify

| File | Change |
|---|---|
| `cli/src/add.rs` | Move `enrich + save_membership` to before `pool_balance_raid1` in the else branch. Keep the existing block in the bootstrap branch. Add file-local `enrich_from_live_pool` helper. |
| `manual/commands/add.md` | Reword "What happens under the hood" step 5/6 to reflect the new order: "adds the device, **records the new membership in `pool.json`**, then balances data to RAID1". |

No changes required to `cli/src/recover.rs`, `cli/src/membership.rs`, or
`cli/src/journal.rs`. Recovery's existing rebuild-from-live-pool path
(`recover.rs:346-393`) becomes naturally idempotent in the common
post-fix case (live pool == pool.json), and the soft-balance replay in
`replay_post_mutation` (`recover.rs:646-668`) is unchanged.

## Tests

### New: `tests/cli/braid-add-persists-before-balance.py` + `.nix`

Single focused regression test for this bug. Asserts that `pool.json`
reflects the new disk *while the post-add balance is still running*,
which is the exact invariant the fix establishes.

```text
Intent: pool.json is written before the post-add balance starts, so
        bookkeeping is consistent with the live btrfs pool the moment
        the balance runs.

Why it exists: incident showed that an interrupted post-add balance
        left pool.json reporting N disks while the live pool had N+1.
        `braid status` then disagreed with pool.json, sending the
        operator into recover docs to reconcile.

Scenario:
  1. Bootstrap a 1-disk pool, write enough data to make the post-add
     balance non-instantaneous.
  2. Run `braid add disk2` in the background.
  3. Poll `btrfs balance status` until the balance is running with
     significant work remaining (reuse the >=70%-remaining pattern
     from tests/module/ups-lb-during-balanced-add.py:95-121).
  4. Snapshot pool.json *now* (balance still running). Parse JSON.
     Assert: contains BOTH disk1 and disk2.
     Assert: disk2 entry has non-null `luks_uuid`, `devid`, `added_at`
             (proves the helper preserves enrichment, not just the
             member entry).
     Assert: pending-op.json still present (op not yet complete).
  5. Wait for braid to finish; assert exit 0, pending-op.json gone.
```

The polling pattern already exists in
`tests/module/ups-lb-during-balanced-add.py`; copy it. Reuse the
existing `pool_json` cat + `assert '"diskN"' in ...` assertion style.

Register in `flake.nix` `checks` set, e.g.:

```nix
braid-add-persists-before-balance = pkgs.testers.nixosTest (
  import ./tests/cli/braid-add-persists-before-balance.nix {
    braid = linuxCrane.braid;
  }
);
```

(Per memory: new VM tests must be wired into `flake.nix`, not just the
justfile -- `just test-vm` dispatches off the checks set.)

### Existing coverage already adequate (no new test needed)

- **Recovery after interrupted post-add balance** is already covered by
  `tests/module/ups-lb-during-balanced-add.py`, which forces a system
  shutdown mid-balance and asserts post-recovery state. After the fix,
  it still passes -- `pool.json` is now correct *before* the shutdown,
  so the recovery rebuild step rewrites the same content (idempotent).
  Do not duplicate that test surface.
- **Bootstrap branch correctness** -- this fix does not change the
  bootstrap branch. Existing `add-bootstrap.py` and `braid-add-disk.py`
  cover it.

### Manual sanity

`just test-rust` should pass without changes (no parser surface
touched).

## Out of scope (flagged for follow-up)

A *similar-looking* "save after long balance" shape appears in:

- `cli/src/remove_missing.rs:190-216` -- post-remove balance
  (`maybe_restore_raid1`) after `pool_remove_device_using`. Likely the
  same fix shape; flag for follow-up.
- `cli/src/replace.rs:370-429` -- post-replace resize + optional
  balance after `pool_replace_device`. Likely the same fix shape; flag
  for follow-up.

`cli/src/remove.rs` is **not** in this list despite a superficial
resemblance. For `remove`, the long operation *is* the device removal
itself (`btrfs device remove` blocks until chunks are evacuated), and
recovery treats `Remove` differently from `Add`/`RemoveMissing`/`Replace`
(see `recover.rs:646-668`'s match arms). Persisting `pool.json` before
`btrfs device remove` finishes would record a disk as removed while
btrfs still owns it -- the inverse of the current bug. Any follow-up on
`remove` requires its own analysis, not a copy-paste of this fix.

Per the task brief, do not fix the flagged commands here. Open
follow-up issues / plans referencing this one.

Also explicitly out of scope (per task brief):

- Trapping SIGINT/SIGHUP in braid to translate to `btrfs balance pause`.
- Replacing recover's `,soft` replay with logic that handles the
  "interrupted-early redistribute" case.
- A new ADR. The fix aligns with existing principles in
  `docs/decisions/017-runtime-disk-membership.md` ("`pool.json`
  reflects completed operations") -- it corrects an ordering bug, not
  an architectural choice. If the user disagrees, an ADR amendment to
  017 clarifying "completed = membership committed, not post-mutation
  balance complete" would be the right venue, but is not load-bearing.

## Verification

1. `just test-rust` -- baseline unit tests pass.
2. `just test-vm braid-add-persists-before-balance` -- new regression
   test passes.
3. `just test-vm ups-lb-during-balanced-add` -- existing
   interrupt-during-balance recovery test still passes.
4. `just test-vm braid-add-disk multi-add add-bootstrap` -- existing
   add tests unaffected.
5. Manual: same shape as the regression test -- bootstrap a 1-disk
   pool, write a few GiB of data so the post-add balance has work to
   do, `braid add disk2`, then `cat /var/lib/braid/pool.json` while
   the single->RAID1 balance is mid-flight; confirm pool.json already
   has both disks. (1->2 is more reliable than 2->3 for catching
   pre-balance state, since 2->3 may converge before inspection.)
