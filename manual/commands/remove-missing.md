[← Manual](../index.md)

# braid remove-missing

Forget a stale missing-device entry from the pool. This does NOT rebuild data -- use [`braid replace`](replace.md) for that.

## When to use it

- A disk has permanently failed and you want to clean up the pool metadata without replacing it
- You have already recovered your data and just need btrfs to stop reporting the missing device

This is a destructive choice: any data that only existed on the missing disk is lost. If you want to rebuild data onto a new disk, use `braid replace` instead.

## Basic example

Note: `braid status` lists every devid that contributes to `missing_count`,
including drives whose LUKS mapper is still open but whose physical device just
disappeared (null-underlying). btrfs has not yet promoted those devids to its
authoritative `MISSING` state, and `remove-missing` operates only on that
authoritative set. If `remove-missing --missing-id N` reports that devid `N` is
not a device in this pool for a devid that `status` reports as missing, that
error itself is the signal -- btrfs has not promoted the devid yet. To make
progress, follow the missing-disk recovery workflow in
[`recovery-scenarios.md`](../guides/recovery-scenarios.md). Typically:
confirm the disk is truly gone, remount the pool degraded if it is not already,
then retry once btrfs reports an authoritative missing device.

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
3. Resolves the devid to a disk name in pool.json
4. Shows a confirmation prompt with the disk name, devid, and resulting pool size
5. Runs `btrfs device remove <devid>` to clear the missing device entry
6. If this was the last missing device and 2+ disks remain: runs a soft RAID1 balance (`-dconvert=raid1,soft -mconvert=raid1,soft`) to restore redundancy on any single-profile chunks created during degraded operation
7. Updates pool.json to remove the disk entry

A sleep inhibitor is held during the removal and the subsequent soft balance (if triggered).

## Safety checks / refusal cases

- Refuses if the pool is not mounted
- Refuses if no missing devices are detected
- Refuses if the specified devid belongs to a live device (use `braid remove` for that)
- Refuses if the specified devid is not a device in this pool
- Refuses if surviving disks lack space to absorb the missing device's allocations (ENOSPC pre-flight), when more than 1 surviving device exists
- Refuses if another braid operation is pending
- Refuses if a btrfs exclusive operation is already running

## Related commands

- [braid status](status.md) -- find missing device IDs
- [braid replace](replace.md) -- replace a missing disk with a new one (rebuilds data)
- [braid remove](remove.md) -- remove a live disk
