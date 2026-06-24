---
experimental: false
---
[← braid](../index.md)

# braid lock

Unmount the btrfs pool and close all LUKS mappers.

## When to use it

- Before shutting down or rebooting (though systemd handles this automatically)
- When you want to manually take the pool offline
- Before physically removing a disk (after `braid remove`)

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

1. Checks if the pool is mounted
2. Checks that no btrfs exclusive operation (balance, device remove, etc.) is running. Skipped when the pool is not mounted.
3. Unmounts the btrfs filesystem, retrying up to 3 times if the device is busy (covers the brief race after stopping SMB/NFS consumers, where the kernel has not yet released the last file descriptors). During systemd shutdown the `ExecStop` path retries far longer -- 60 attempts over roughly 30 seconds -- so a btrfs-progs process that still briefly holds the mount fd after its parent is killed does not strand the pool with LUKS open; see [ADR-018](../design/decisions/018-systemd-lifecycle.md#execstop-bounded-wait-pattern).
4. After a successful unmount, runs `btrfs device scan --forget` for the planned close-set mappers (member-owned plus any orphaned `braid-*` mappers from a prior crash) that still exist on disk, clearing the kernel's device registry so stale references do not race with mapper close. Skipped when there is nothing left to forget.
5. Classifies live mappers by LUKS UUID/devid ownership, then closes member-owned observed mapper names, retrying up to 3 times if the device is busy
6. Scans for orphaned `braid-*` mappers not owned by UUID-keyed membership (e.g. from a prior crash) and closes those too

If the pool is already unmounted and all mappers are already closed, lock reports "pool already locked" and exits cleanly.

### On NixOS module installs

When braid is installed via the NixOS module, `braid lock` also:

- Stops `braid-scrub.timer`, `braid-scrub-resume-trigger.service`, and `braid-scrub.service` before unmount.
- Stops any consumer wired into the pool lifecycle via `BindsTo=braid-online.service` (lock walks its reverse, `BoundBy`; e.g. an SMB or NFS unit you set up that way -- see [Sharing and permissions](../guides/sharing-and-permissions.md)) before unmount.
- Stops `braid-online.service` itself after a successful unmount.

`braid unlock` reverses the third step: it reactivates `braid-online.service` after mount, which restarts every consumer that is also `WantedBy=braid-online.service`. A consumer wired with only one of the two half-works -- `BindsTo` stops it before lock, `WantedBy` restarts it after unlock -- so wire both; the sharing guide shows the full setup.

Standalone CLI installs (no NixOS module) skip all three -- there is no `braid-online.service` or scrub unit to stop.

## Error handling

- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
- If unmount fails after exhausting its umount busy-retry budget (e.g. a process has files open on the mount), lock skips `btrfs device scan --forget` and still attempts to close the LUKS mappers, reporting the failure
- If a mapper close fails with "device busy" after unmount also failed, the error is downgraded to a warning (the root cause is likely the stuck unmount)
- The hint `lsof <mount_point>` or `fuser -vm <mount_point>` is printed when unmount fails, to help identify the blocking process
- If a scanned `braid-*` mapper's backing LUKS UUID cannot be verified (for example because its backing device is gone or its LUKS header is unreadable), lock prints a `[warn]`, leaves that mapper open instead of closing it, excludes it from both `btrfs device scan --forget` and the close step, and still exits cleanly. Re-run `braid lock` once the mapper's LUKS UUID is readable. The literal `cleanup incomplete` summary line appears only under `--dry-run`; a real run surfaces the per-mapper `[warn]` and does not print `pool already locked`. See [ADR-024](../design/decisions/024-luks-uuid-identity.md#runtime-handles-and-labels).

## Related commands

- [braid unlock](unlock.md) -- open LUKS devices and mount the pool
- [braid status](status.md) -- check pool state before locking
- [braid idle](idle.md) -- check if operations are running before locking
