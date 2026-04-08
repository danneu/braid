# Fix: Move pool.json write in `braid add` to after disk operations succeed

**Status: Superseded** by `cheerful-prancing-hearth.md` — all mutations now use post-commit persist with journal. Pool.json is written only after disk operations succeed.

## Context

`add.rs:339-344` writes pool.json (authoritative membership) BEFORE LUKS format and btrfs device add. If any subsequent step fails (luksFormat, header backup, cryptsetup open, btrfs device add), pool.json retains a stale entry for a disk that never joined the pool. On next `braid unlock`, the stale entry is treated as a missing member — requiring `--allow-degraded` for what was just a transient add failure. This also breaks repeatability of the systemd entry point (`systemctl start braid-pool.target`), since its behavior now depends on whether a previous `braid add` failed.

## Fix

Move the `membership::save_membership()` call from before disk operations to after all disk operations succeed.

### Current flow (`add.rs`):
```
validate → write pool.json (line 339) → LUKS phase → pool phase → disk-map update
```

### New flow:
```
validate → LUKS phase → pool phase → write pool.json → disk-map update
```

### Trade-off:

| Failure scenario | Current (pre-commit) | Fixed (post-commit) |
|---|---|---|
| Disk op fails | pool.json has stale entry → unlock broken, manual repair | pool.json unchanged → clean state |
| pool.json write fails | never touches disks (clean) | disk in pool but not in pool.json → `braid discover --write` recovers |

Post-commit is strictly better: the stale-entry failure has no automatic recovery, while the missing-entry failure has a built-in recovery command.

## Changes

### 1. `cli/src/add.rs` — move membership write

**Delete** lines 339-344 (pre-commit persist block):
```rust
// Pre-commit persist: save membership after all reversible checks pass,
// but before the first irreversible disk operation (LUKS format).
for (name, by_id) in &parsed {
    pool_membership.disks.insert(name.clone(), by_id.clone());
}
membership::save_membership(&pool_membership, paths)?;
```

**Insert** equivalent block after pool phase completes (after current line 509, before the disk-map update):
```rust
// Post-commit persist: save membership only after all disk operations
// (LUKS format + pool add/bootstrap) have succeeded.  If a disk op
// fails, pool.json is never touched — no stale entries.
for (name, by_id) in &parsed {
    pool_membership.disks.insert(name.clone(), by_id.clone());
}
membership::save_membership(&pool_membership, paths)?;
```

All variables (`parsed`, `pool_membership`, `paths`) remain in scope. Error propagation via `?` is unchanged (`MembershipError` → `AddError` conversion already exists at line 38).

**Early-return path** (lines 449-457, `needs_pool_add.is_empty()`): No pool.json write needed — if all disks are already in pool, pool.json already has them.

### 2. `docs/decisions/017-runtime-disk-membership.md` — update mutation ordering

Line 31, change "validate → persist membership → irreversible disk operation" to "validate → irreversible disk operation → persist membership". Update the rationale line (33) to explain the post-commit approach.

### 3. New test: `tests/cli/add-membership-no-stale-on-failure.{nix,py}`

Proves the bug: a failed `braid add` must not leave a stale pool.json entry.

**Approach:** `blockdev --setro` makes disk3 read-only at the kernel level → `cryptsetup luksFormat` fails with I/O error → braid add exits non-zero.

**Steps:**
1. Build 2-drive RAID1 pool (disk1 + disk2)
2. `blockdev --setro /dev/disk/by-id/virtio-disk3`
3. `braid add disk3=...` — fails (LUKS format can't write)
4. Assert pool.json does NOT contain disk3
5. Lock the pool
6. `braid unlock` succeeds WITHOUT `--allow-degraded`
7. Data integrity check
8. Restore disk3 read-write, add it successfully (proves no leftover state)

### 4. `flake.nix` — register new test

Add after `remove-missing-membership-readonly` (line 319):
```nix
add-membership-no-stale-on-failure = pkgs.testers.nixosTest (
  import ./tests/cli/add-membership-no-stale-on-failure.nix {
    braid = linuxCrane.braid;
  }
);
```

## Related: `replace.rs` has the same bug

`replace.rs:196-202` has the identical pre-commit pattern — saves membership swap before LUKS format. If LUKS format fails, pool.json has the old disk removed and the new disk added, but neither operation actually happened in btrfs. This is a separate fix (different risk profile: replace swaps entries rather than adding) and should be a follow-up.

## Verification

1. Write test first, run `just test add-membership-no-stale-on-failure` — should **fail** (pool.json contains disk3)
2. Apply Rust fix, re-run — should **pass**
3. Run existing `just test braid-add-disk` to verify normal add flow still works
4. `just test-rust` for unit tests
