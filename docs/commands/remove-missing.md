[← braid](../index.md)

# braid remove-missing

Forget a stale missing-device entry from the pool. This does NOT rebuild data -- use [`braid replace`](replace.md) for that.

## When to use it

- A disk has permanently failed and you want to clean up the pool metadata without replacing it
- You have already recovered your data and just need btrfs to stop reporting the missing device

This is a destructive choice: any data that only existed on the missing disk is lost. If you want to rebuild data onto a new disk, use `braid replace` instead.

## Basic example

Note: `braid remove-missing` operates only on btrfs-authoritative `MISSING`
devids. A drive that is hot-unplugged while the pool is mounted contributes to
`missing_count` and appears in `missing_devids` in `braid status` before btrfs
promotes its devid to `MISSING`; `remove-missing` refuses the devid with a
specific hot-unplug diagnostic until that promotion happens. See
[Hot-unplug while pool is mounted](../guides/recovery-scenarios.md#hot-unplug-while-pool-is-mounted).

First, find the missing device's ID:

```
sudo braid status
```

Then remove it:

```
sudo braid remove-missing --missing-id 3
```

## Common variations

Preview what would happen:

```
sudo braid remove-missing --missing-id 3 --dry-run
```

Skip the confirmation prompt:

```
sudo braid remove-missing --missing-id 3 --yes
```

## Important flags

| Flag | Purpose |
|---|---|
| `--missing-id <devid>` | Target missing device by btrfs devid (required) |
| `--dry-run` | Show what would happen without executing |
| `--yes` | Skip interactive confirmation |
| `--progress auto\|on\|off` | Control progress display (default: auto) |

## What happens under the hood

1. Probes the pool to verify missing devices exist
2. Validates that the specified devid is actually a missing device (not a live one)
3. Resolves the btrfs devid to the UUID-keyed pool member whose persisted prior devid matches
4. Shows a confirmation prompt with the disk name, devid, and resulting pool size
5. Writes a `PoolMutation` journal and runs `btrfs device remove <devid>` to clear the missing device entry
6. Updates pool.json to remove the member's UUID entry, then advances the journal to post-remove-missing maintenance
7. If this was the last missing device and 2+ disks remain: runs a soft RAID1 balance (`-dconvert=raid1,soft -mconvert=raid1,soft`) to restore redundancy on any single-profile chunks created during degraded operation
8. Clears the journal

A sleep inhibitor is held during the removal and the subsequent soft balance (if triggered).

If a btrfs exclusive operation (a running balance, device add/remove/replace, resize, or swap activate) is already in flight on the pool, braid does not fail -- its `btrfs` commands queue behind the in-flight operation (via `--enqueue`) and the kernel runs them when the pool is free. A *paused* balance is the exception and is refused (see Safety checks below).

## Safety checks / refusal cases

- Refuses if the pool is not mounted
- Refuses if no missing devices are detected
- Refuses if the specified devid belongs to a live device (use `braid remove` for that)
- Refuses if the specified devid is not a device in this pool
- Refuses if surviving disks lack space to absorb the missing device's
  allocations (ENOSPC pre-flight), or if that pre-flight cannot run (the
  `btrfs device usage` probe failed to spawn, returned a nonzero exit,
  produced unparseable output, did not list the targeted missing devid, or
  reported an untrusted missing-device allocation shape: the targeted devid is
  listed more than once, carries an allocation profile braid does not model, or
  reports no positive Data/Metadata/System RAID1 row), when more than 1
  surviving device exists
- Refuses on a 2-disk RAID1 pool with one disk missing -- the kernel refuses to drop a RAID1 pool below two devices. Use `braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>` to repair the dead disk, or `braid add` first and then re-run.
- Refuses if a pending operation journal (`pending-op.json`) exists -- run `braid recover` to reconcile.
- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
- Refuses if a btrfs balance is *paused* on the pool -- resume or cancel it first. A paused balance holds the exclusive-operation lock indefinitely, so braid cannot wait it out.

## Related commands

- [braid status](status.md) -- find missing device IDs
- [braid replace](replace.md) -- replace a missing disk with a new one (rebuilds data)
- [braid remove](remove.md) -- remove a live disk
