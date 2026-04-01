# Plan: VM test for crash recovery during `braid remove`

## Context

The existing `braid-recover.py` VM test only covers `OpKind::Add` recovery. The recovery code in `recover.rs` is topology-driven and OpKind-agnostic — it probes live btrfs and rebuilds pool.json from whatever it finds. But the *pool state* that recovery encounters differs by OpKind, and Remove has a specific subtlety: the removed disk's LUKS container still exists on the physical device, so `union_memberships` opens it during recovery, but `probe_pool` must correctly exclude it from the rebuilt membership.

This test closes that gap by exercising both crash timing points for Remove.

## Files

| File | Action |
|---|---|
| `tests/cli/braid-recover-remove.nix` | Create — VM config (3 disks) |
| `tests/cli/braid-recover-remove.py` | Create — test script |
| `flake.nix` | Edit — register test near line 309 |

## Test design

One test file, two independent scenarios. Each scenario builds its own fresh 3-disk pool so they're independently reproducible — if Scenario A regresses, Scenario B still runs cleanly.

### Scenario A: Crash before `btrfs device remove`

Pool still has 3 disks in btrfs. Recovery probes topology → sees 3 devices → rebuilds pool.json with all 3.

1. Build 3-disk RAID1 pool, write test data, capture pool.json
2. `braid lock`
3. Inject `pending-op.json` with nested op format: `"op": {"op": "Remove", "name": "disk3"}`, `pre_membership` = 3-disk pool.json, `target_membership` = pool.json with disk3 removed
4. Verify `braid unlock` refuses ("interrupted operation")
5. `braid recover --passphrase-stdin`
6. Assert: pool.json has disk1+disk2+disk3, journal cleared, data intact
7. Lock/unlock cycle works

### Teardown + rebuild between scenarios

After Scenario A, tear down the pool completely (lock, wipe all disks) and rebuild a fresh 3-disk pool for Scenario B. This costs ~30s of extra setup but makes each scenario a clean, independent reproduction.

### Scenario B: Crash after `btrfs device remove` but before pool.json write

Pool has 2 disks in btrfs (disk3 evicted). disk3's LUKS container still exists on the physical device. Recovery opens all 3 LUKS devices via `union_memberships`, probes btrfs topology → sees 2 devices → rebuilds pool.json with disk1+disk2 only.

1. Build fresh 3-disk RAID1 pool, write test data, capture pool.json
2. Manually evict disk3 from btrfs: `btrfs device remove /dev/mapper/braid-disk3 /mnt/storage`
3. Close disk3 LUKS: `cryptsetup close braid-disk3`
4. Verify `btrfs fi show` confirms only disk1+disk2 remain
5. `braid lock`
6. Inject `pending-op.json` with same nested op format as Scenario A
7. `braid recover --passphrase-stdin`
8. Assert: pool.json has disk1+disk2 only (disk3 excluded), journal cleared, data intact
9. **Assert: `/dev/mapper/braid-disk3` exists** — recovery opened disk3's LUKS via the union membership even though it was excluded from the rebuilt pool.json. This is the specific edge: recovery tolerates an openable-but-no-longer-member disk.
10. Lock/unlock cycle works

### Key details

- **Nested journal op format.** `Journal` has a top-level `op` field (journal.rs:16) whose value is an internally-tagged `OpKind` (journal.rs:24-25). Serialized: `{"op": {"op": "Remove", "name": "disk3"}, ...}`. Match the existing `braid-recover.py` shape (line 60-67) which uses this same nested pattern for Add.
- **No `--allow-degraded` needed.** All physical disks are present in both scenarios. In Scenario B, disk3's LUKS container exists on the physical device — `open_and_mount_pool` opens it without error (it's not absent/damaged), `probe_pool` simply doesn't include it.
- **`braid lock` handles already-closed mappers.** `lock.rs:133` checks `fs.exists(&mapper_path)` before close, so disk3 being already closed is a no-op.
- **Scenario B's distinctive assertion.** After recovery, check that `braid-disk3` mapper is open (recovery opened it via union) while pool.json omits disk3 (btrfs topology excluded it). This proves recovery tolerates the openable-but-excluded disk, not just that the final membership is correct.

### .nix config

Identical to `braid-recover.nix` but with 3 × 1024 MB disks (disk1, disk2, disk3).

### flake.nix registration

```nix
braid-recover-remove = pkgs.testers.nixosTest (
  import ./tests/cli/braid-recover-remove.nix {
    braid = linuxCrane.braid;
  }
);
```

## Verification

```
just test braid-recover-remove
```

On failure, re-run with `-v` on that single test for VM logs.
