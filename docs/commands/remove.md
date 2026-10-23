---
experimental: false
---
[← braid](../index.md)

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
| `--progress auto\|always\|never` | Control progress display (default: auto) |

## What happens under the hood

1. Probes the pool to verify the disk is a live member
2. Checks that the remaining disks have room for the data being migrated (see the space rule under Safety checks below)
3. Shows a confirmation prompt with the disk's name and devid, its model/size/serial (best-effort from the live backing device via lsblk -- omitted if unavailable), and the resulting disk count (e.g. `Pool: 3 disks -> 2 disks`). When one disk would remain, the prompt also carries a `WARNING:` line that the pool loses RAID1 redundancy. This interactive prompt, warning included, is skipped by `--yes` and not shown in `--dry-run`; dry-run shows the redundancy loss as the balance step of step 4 instead (see [ADR 022](../design/decisions/022-dry-run-preview-model.md#confirmation-ui)).
4. If removing the second-to-last disk (going from 2 to 1): first balances the pool off RAID1, then removes the device. Data converts to single, and metadata and system convert to DUP (two copies on the one disk), which `braid status` then shows. See [btrfs balance profiles](../internals/btrfs/balance-profiles.md) for why metadata keeps two copies.
5. Runs `btrfs device remove` to migrate all data off the disk (this is the long-running step)
6. Closes the LUKS mapper on the removed disk
7. Updates pool.json to remove the member's UUID entry

The step-6 mapper close is best-effort. Because btrfs has already committed the device removal, braid never fails the remove on the close: a failed close prints a `[warn] disk <name>: lock failed` row, and a close-time ownership probe that cannot prove the mapper is the expected disk -- inactive, a probe failure, or a foreign LUKS UUID -- skips the close with a `Warning:` line. A clean exit therefore does not guarantee the mapper is closed. The probe reads the LUKS UUID the kernel recorded when the mapper was opened, not the disk's header, so the mapper still closes when the removed disk is already gone.

A sleep inhibitor is held during data migration and cleanup.

If a btrfs exclusive operation (a running balance, device add/remove/replace, resize, or swap activate) is already in flight on the pool, braid does not fail -- its `btrfs` commands queue behind the in-flight operation (via `--enqueue`) and the kernel runs them when the pool is free. A *paused* balance is the exception and is refused (see Safety checks below).

## Safety checks / refusal cases

- Refuses if the pool is not mounted
- Refuses if the named disk is hot-unplugged: its LUKS mapper is open but `cryptsetup status` reports `device: (null)`, and btrfs has not promoted it to MISSING. Re-plugging alone never reopens the mapper, so braid gives the reopen route: `braid lock` then `braid unlock` if the disk is back, or `braid lock` then `braid unlock --allow-degraded` if it is gone, then re-run `braid remove`. If the disk is hot-unplugged after the journal is written, the route reopens with `braid recover` instead. See [Hot-unplug while pool is mounted](../guides/recovery-scenarios.md#hot-unplug-while-pool-is-mounted).
- Refuses if the named disk is otherwise not a live member of the pool. If missing devices are detected, it suggests `braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>`, and also `braid remove-missing` unless that command would refuse the pool's shape (the last missing disk of a 2-disk RAID1 pool)
- Refuses to remove the last disk from the pool
- Refuses if there are missing devices in the pool (resolve those first)
- Refuses if the remaining disks lack space for the removed disk's data
  (ENOSPC pre-flight). When one disk would remain, braid also refuses
  whenever it cannot verify the survivor's space. When two or more would
  remain and braid cannot run `btrfs device usage` or read its output, it
  prints a `[warn]` row and goes ahead, because btrfs then stops cleanly on
  ENOSPC. A nonzero exit from btrfs refuses on both paths. The error names
  the reason. See
  [ADR 012](../design/decisions/012-intent-cli.md#enospc-pre-flight-check)
  for the policy.
{{#include ../_includes/mutating-command-refusals.md.inc}}

## Related commands

- [braid status](status.md) -- see which disks are in the pool and their devids
- [braid remove-missing](remove-missing.md) -- forget a dead/missing device entry
- [braid replace](replace.md) -- replace a disk (live or dead) with a new one
- [braid add](add.md) -- add a disk to the pool
