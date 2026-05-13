[← Manual](../index.md)

# braid remove

Remove a live disk from the pool. Data migrates off the disk before it is detached.

## When to use it

- Shrinking the pool (retiring a drive you no longer need)
- Removing a drive that is still online and healthy

If the disk is already dead or missing, use [`braid replace`](replace.md) to rebuild data onto a new disk, or [`braid remove-missing`](remove-missing.md) to forget the entry without rebuilding.

## Basic example

```
sudo braid remove toshiba3
```

## Common variations

Preview what would happen:

```
sudo braid remove toshiba3 --dry-run
```

Skip the confirmation prompt:

```
sudo braid remove toshiba3 --yes
```

## Important flags

| Flag | Purpose |
|---|---|
| `--dry-run` | Show what would happen without executing |
| `--yes` | Skip interactive confirmation |
| `--progress auto\|on\|off` | Control progress display (default: auto) |

## What happens under the hood

1. Probes the pool to verify the disk is a live member
2. Checks that remaining disks have enough free space to absorb the data being migrated
3. Shows a confirmation prompt with the disk's name, model, serial, devid, and the resulting pool size
4. If removing the second-to-last disk (going from 2 to 1): first balances the pool from RAID1 to single profile, then removes the device
5. Runs `btrfs device remove` to migrate all data off the disk (this is the long-running step)
6. Closes the LUKS mapper on the removed disk
7. Updates pool.json to remove the member's UUID entry

A sleep inhibitor is held during data migration and cleanup.

## Safety checks / refusal cases

- Refuses if the pool is not mounted
- Refuses if the named disk is not a live member of the pool (suggests `braid replace` or `braid remove-missing` if missing devices are detected)
- Refuses to remove the last disk from the pool
- Refuses if there are missing devices in the pool (resolve those first)
- Refuses if remaining disks lack space to absorb the removed disk's data (ENOSPC pre-flight)
- Warns when removal leaves a single disk (no RAID1 redundancy)
- Refuses if another braid operation is pending
- Refuses if a btrfs exclusive operation is already running

## Related commands

- [braid status](status.md) -- see which disks are in the pool and their devids
- [braid remove-missing](remove-missing.md) -- forget a dead/missing device entry
- [braid replace](replace.md) -- replace a disk (live or dead) with a new one
- [braid add](add.md) -- add a disk to the pool
