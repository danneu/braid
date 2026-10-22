---
experimental: false
---
[← braid](../index.md)

# braid lock

Unmount the btrfs pool and close all LUKS mappers.

## When to use it

- Before shutting down or rebooting (though systemd handles this automatically)
- When you want to manually take the pool offline

## Basic example

```
sudo braid lock
```

## Common variations

Preview what would happen:

```
sudo braid lock --dry-run
```

## Important flags

| Flag        | Purpose                                  |
| ----------- | ---------------------------------------- |
| `--dry-run` | Show what would happen without executing |

## What happens under the hood

1. Checks if the pool is mounted, and that the filesystem at the mount point is this pool: a btrfs whose device records the LUKS UUID of a disk in `pool.json` or in the pending-operation journal. When braid cannot tell (for example, `pool.json` is corrupt, or was deleted while no operation is pending, or the kernel's device-mapper entry for the device cannot be read), lock treats the mount as the pool, so a missing record never blocks cleanup.
2. Checks that no btrfs exclusive operation (balance, device remove, etc.) is running. Skipped when the pool is not mounted. A paused balance does not block lock: lock prints a `[warn]` note and unmounts, and the balance stays paused on disk. The next `braid unlock` and `braid doctor` report it.
3. Unmounts the btrfs filesystem, retrying briefly if the device is busy (covers the brief race after stopping SMB/NFS consumers, where the kernel has not yet released the last file descriptors). During shutdown it retries longer; see [ADR-018](../design/decisions/018-systemd-lifecycle.md#execstop-bounded-wait-pattern).
4. After a successful unmount, runs `btrfs device scan --forget` for the planned close-set mappers (member-owned plus any orphaned `braid-*` mappers from a prior crash) that still exist on disk, clearing the kernel's device registry so stale references do not race with mapper close. Skipped when there is nothing left to forget.
5. Closes each member's mapper, retrying briefly if the device is busy. Lock finds members by LUKS UUID, not by mapper name, so it closes a member's mapper even when it is open under another name; see [ADR-024](../design/decisions/024-luks-uuid-identity.md#concrete-improvements).
6. Scans for orphaned `braid-*` mappers not owned by UUID-keyed membership (e.g. from a prior crash) and closes those too

If the pool is already unmounted and all mappers are already closed, lock reports "pool already locked" and exits cleanly.

### On NixOS module installs

When braid is installed via the NixOS module, `braid lock` also:

- Stops `braid-scrub.timer` and `braid-scrub.service` before unmount.
- Stops any consumer wired into the pool lifecycle via `braid.poolBoundServices` or a direct `BindsTo=braid-online.service` edge (lock walks its reverse, `BoundBy`; e.g. SMB or NFS -- see [Sharing and permissions](../guides/sharing-and-permissions.md)) before unmount.
- Stops `braid-online.service` itself whenever the pool ends unmounted, even if a mapper close then fails.

`braid unlock` reverses the third step: it reactivates `braid-online.service` after mount, which restarts every consumer configured through `braid.poolBoundServices` or otherwise wired with `WantedBy=braid-online.service`. A consumer wired with only one of the two half-works -- `BindsTo` stops it before lock, `WantedBy` restarts it after unlock -- so use `braid.poolBoundServices` for long-running pool consumers; the sharing guide shows the full setup.

Standalone CLI installs (no NixOS module) skip all three -- there is no `braid-online.service` or scrub unit to stop.

On module installs, lock stops the scrub units and consumers only after every check in steps 1-2 has passed, right before unmount. So a lock that refuses, for example because a balance is running, leaves scrub and your shares running. `braid lock --dry-run` lists these stops as `stop <unit>` steps.

## Error handling

- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
- Refuses if a running balance or another btrfs exclusive operation is in progress -- wait for it to finish, then re-run `braid lock`. A paused balance does not block lock (step 2).
- Refuses if the filesystem at the mount point is not btrfs, or is a btrfs that is provably not this pool (its device is not a LUKS mapper, or its LUKS UUID belongs to no disk braid knows). Lock then unmounts nothing and closes no mapper; check what is mounted there.
- If unmount fails after exhausting its umount busy-retry budget (e.g. a process has files open on the mount), lock skips `btrfs device scan --forget` and still attempts to close the LUKS mappers, reporting the failure
- If a mapper close fails with "device busy" after unmount also failed, the error is downgraded to a warning (the root cause is likely the stuck unmount)
- If unmount succeeds but a mapper close then fails (e.g. a process still holds a `/dev/mapper/braid-*` device open), lock exits 1 with the pool unmounted and that mapper open. On module installs it still stops `braid-online.service`, so the next `braid unlock` restarts your shares and scrub. Free the holder and re-run `braid lock` to close the leftover mapper.
- The hint `lsof <mount_point>` or `fuser -vm <mount_point>` is printed when unmount fails, to help identify the blocking process
- A member's mapper still closes when its disk has died or been unplugged: lock reads the LUKS UUID the kernel recorded when the mapper was opened, not the disk's header.
- If a scanned `braid-*` mapper carries no LUKS UUID lock can read (for example a stale `/dev/mapper` entry with no device behind it, or a mapper opened outside braid without LUKS), lock prints a `[warn]`, leaves that mapper open instead of closing it, excludes it from both `btrfs device scan --forget` and the close step, and still exits cleanly. Check what it is with `cryptsetup status <mapper>` before closing it by hand. The literal `cleanup incomplete` summary line appears only under `--dry-run`; a real run surfaces the per-mapper `[warn]` and does not print `pool already locked`. See [ADR-024](../design/decisions/024-luks-uuid-identity.md#runtime-handles-and-labels).

## Related commands

- [braid unlock](unlock.md) -- open LUKS devices and mount the pool
- [braid status](status.md) -- check pool state before locking
- [braid idle](idle.md) -- check if operations are running before locking
