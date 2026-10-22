---
experimental: false
---
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
specific hot-unplug diagnostic until that promotion happens. Until then it also
refuses every other devid, because btrfs still writes to the unplugged disk. See
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
| `--progress auto\|always\|never` | Control progress display (default: auto) |

## What happens under the hood

1. Probes the pool to verify missing devices exist
2. Validates that the specified devid is actually a missing device (not a live one), and that no other member is hot-unplugged but not yet `MISSING`
3. Resolves the btrfs devid to the UUID-keyed pool member whose recorded devid matches. braid does not probe a MISSING device through its mapper, so it has no observable LUKS UUID and its recorded devid is the only binding
4. Shows a confirmation prompt with the disk name, devid, and the resulting disk counts (e.g. `Pool: 2 present + 1 missing -> 2 disks`). This interactive prompt is skipped by `--yes` and not shown in `--dry-run`.
5. After confirmation (or straight away with `--yes`), plans again against the live pool: steps 1-3 and the pool's safety checks below. If a check now refuses, or the new plan differs from the confirmed one, braid stops before writing anything. This runs before any `--enqueue` wait (see below), so it does not cover space used while the removal is queued behind another exclusive operation, or while it runs.
6. Writes a `PoolMutation` journal and runs `btrfs device remove <devid>` to clear the missing device entry
7. Updates pool.json to remove the member's UUID entry, then advances the journal to post-remove-missing maintenance
8. If this was the last missing device and 2+ disks remain: runs a soft RAID1 balance (`-dconvert=raid1,soft -mconvert=raid1,soft`) to restore redundancy on any single-profile chunks created during degraded operation
9. Clears the journal

A sleep inhibitor is held during the removal and the subsequent soft balance (if triggered).

If a btrfs exclusive operation (a running balance, device add/remove/replace, resize, or swap activate) is already in flight on the pool, braid does not fail -- its `btrfs` commands queue behind the in-flight operation (via `--enqueue`) and the kernel runs them when the pool is free. A *paused* balance is the exception and is refused (see Safety checks below).

## Safety checks / refusal cases

- Refuses if the pool is not mounted
- Refuses if no missing devices are detected
- Refuses if the specified devid belongs to a live device (use `braid remove` for that)
- Refuses if the specified devid is not a device in this pool
- Refuses while any other member is hot-unplugged but btrfs has not yet
  promoted it to `MISSING` (its LUKS mapper is open and its backing disk is
  gone). btrfs still writes new chunk copies to such a member, so the removal
  could put relocated data on a disk that is gone. Confirm the disk is gone,
  run `braid lock` then `braid unlock --allow-degraded` so btrfs promotes it,
  and retry.
- Refuses if pool.json has no member recorded at the specified devid.
  pool.json may need manual repair; run `braid status` to inspect it.
- When two or more present disks survive, refuses if they lack space for the
  missing device's allocations, or if braid cannot verify that from
  `btrfs device usage`. The error names the reason. See
  [ADR 012](../design/decisions/012-intent-cli.md#enospc-pre-flight-check)
  for the rules.
- Refuses on a 2-disk RAID1 pool with one disk missing -- the kernel refuses to drop a RAID1 pool below two devices. Use `braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>` to repair the dead disk, or `braid add` first and then re-run.
- Refuses if the pool changed between planning and the journal write (for example, survivor space was used up or another disk dropped out while the prompt waited). Nothing was written, so re-run the command.
{{#include ../_includes/mutating-command-refusals.md.inc}}

## Related commands

- [braid status](status.md) -- find missing devids
- [braid replace](replace.md) -- replace a missing disk with a new one (rebuilds data)
- [braid remove](remove.md) -- remove a live disk
