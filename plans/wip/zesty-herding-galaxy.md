# Plan: VM tests for crash recovery during Replace

## Context

The existing `braid recover` VM test (`tests/cli/braid-recover.py`) only injects an `Add` journal. Replace has the most complex intermediate state — the journal union spans both old and new devices — and the recover code path is completely untested with `OpKind::Replace`. A bug in by_id resolution or membership rebuild during replace recovery could silently corrupt pool.json (e.g. wrong by_id for disk4), breaking subsequent unlock/lock cycles.

## Approach

Two focused VM tests, one per crash scenario. Each test is self-contained with its own pool setup — failures are isolated and independently diagnosable.

## Files

| File | Action |
|---|---|
| `tests/cli/recover-replace-not-started.nix` | Create — 4-disk VM config |
| `tests/cli/recover-replace-not-started.py` | Create — crash before replace started |
| `tests/cli/recover-replace-completed.nix` | Create — 4-disk VM config |
| `tests/cli/recover-replace-completed.py` | Create — crash after replace completed |
| `flake.nix` ~line 314 | Add 2 registration entries |

## Test 1: recover-replace-not-started

Crash before replace started. Pool still has {disk1, disk2, disk3}. disk4 is untouched (no LUKS).

### Phases
1. `braid add` disk1, disk2, disk3. Write test data. Capture pool.json snapshot.
2. `braid lock`.
3. Construct target_membership: copy pool.json disks, remove disk2, add disk4 with `{"by_id": "/dev/disk/by-id/virtio-disk4"}`.
4. Inject pending-op.json with `op: Replace { old_name: "disk2", new_name: "disk4", new_by_id: ... }`.
5. Assert `braid unlock` refuses ("interrupted operation").
6. `braid recover --passphrase-stdin --allow-degraded`.
   - `--allow-degraded` needed: disk4 has no LUKS container → `PresentNotLuks` → `any_missing_member`.
7. Assert recovered pool.json:
   - Contains disk1, disk2, disk3 with correct by_id paths (`/dev/disk/by-id/virtio-disk1`, etc.)
   - Does NOT contain disk4
8. Assert journal cleared, data intact.
9. Lock/unlock cycle to confirm normal ops resume.

## Test 2: recover-replace-completed

Crash after `btrfs replace` completed but before pool.json was written. Pool genuinely has {disk1, disk3, disk4} in btrfs, but metadata says {disk1, disk2, disk3}.

### Phases
1. `braid add` disk1, disk2, disk3. Write test data. Capture pool.json snapshot.
2. `braid replace --old disk2 --new disk4` (real replace — pool is now {disk1, disk3, disk4}).
3. `braid lock`.
4. Overwrite pool.json with pre-replace snapshot {disk1, disk2, disk3}.
5. Inject pending-op.json with same Replace journal as test 1.
6. Assert `braid unlock` refuses.
7. `braid recover --passphrase-stdin` (no `--allow-degraded` — all 4 LUKS containers open; disk2's LUKS header survives `btrfs replace`).
8. Assert recovered pool.json:
   - Contains disk1 with by_id `/dev/disk/by-id/virtio-disk1`
   - Contains disk3 with by_id `/dev/disk/by-id/virtio-disk3`
   - Contains disk4 with by_id `/dev/disk/by-id/virtio-disk4` (resolved from target_membership in the union)
   - Does NOT contain disk2
9. Assert journal cleared, data intact.
10. Lock/unlock cycle to confirm normal ops resume.

## Key Technical Details

**mount_key safety**: `open_and_mount_pool` iterates union via BTreeMap (sorted). `to_unlock.first()` picks "disk1" — always a real pool member in both tests. Mount succeeds.

**Test 2 no `--allow-degraded`**: After real replace, disk2's underlying device still has its LUKS container (`btrfs replace` operates at the btrfs layer, not LUKS). All 4 LUKS containers open. btrfs mounts via disk1 and assembles {disk1, disk3, disk4}. disk2's mapper sits unused. `probe_pool` → `btrfs filesystem show /mnt/storage` sees only the 3 real members.

**Journal JSON format** (serde internally-tagged):
```json
{
  "started_at": "2026-01-01T00:00:00Z",
  "op": {
    "op": "Replace",
    "old_name": "disk2",
    "new_name": "disk4",
    "new_by_id": "/dev/disk/by-id/virtio-disk4"
  },
  "pre_membership": "<captured pool.json>",
  "target_membership": "<disk2 removed, disk4 added>"
}
```

**target_membership construction**: Deep-copy pool.json `disks`, delete `"disk2"`, insert `"disk4": {"by_id": "/dev/disk/by-id/virtio-disk4"}`. Matches `build_replacement_membership` in replace.rs which uses `DiskMember::from_by_id` (only `by_id` field, optional fields absent).

**by_id assertions**: Both tests must assert the exact by_id string for each recovered disk member, not just name presence. This is the core value — verifying that `union_memberships` correctly maps live btrfs devices to their by_id paths from the journal snapshots.

## Reusable Patterns

- `.nix` boilerplate: copy from `tests/cli/braid-recover.nix`, add disk3 + disk4
- `.py` structure: follow `braid-recover.py` — `add_cmd` helper, `shlex.quote`, journal injection via heredoc, `json.dumps`
- `replace_cmd` helper: copy from `tests/cli/replace-live-disk.py`
- flake.nix registration: same 3-line block as `braid-recover` entry at line 310

## Verification

```
just test recover-replace-not-started recover-replace-completed
```

If either fails, add `-v` to the specific failing test for VM logs.
