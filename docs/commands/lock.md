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
2. Checks that no btrfs exclusive operation (balance, device remove, etc.) is running
3. Unmounts the btrfs filesystem, retrying up to 3 times if the device is busy (covers the brief race after stopping SMB/NFS consumers, where the kernel has not yet released the last file descriptors)
4. Runs `btrfs device scan --forget` to clear the kernel's device registry (prevents stale references from racing with mapper close)
5. Classifies live mappers by LUKS UUID/devid ownership, then closes member-owned observed mapper names, retrying up to 3 times if the device is busy
6. Scans for orphaned `braid-*` mappers not owned by UUID-keyed membership (e.g. from a prior crash) and closes those too

If the pool is already unmounted and all mappers are already closed, lock reports "pool already locked" and exits cleanly.

### On NixOS module installs

When braid is installed via the NixOS module, `braid lock` also:

- Stops `braid-scrub.timer`, `braid-scrub-resume-trigger.service`, and `braid-scrub.service` before unmount.
- Stops any consumer wired into the pool lifecycle via `BindsTo=braid-online.service` (e.g. an SMB or NFS unit you set up that way -- see [Sharing and permissions](../guides/sharing-and-permissions.md)) before unmount.
- Stops `braid-online.service` itself after a successful unmount.

`braid unlock` reverses the third step: it reactivates `braid-online.service` after mount, which restarts every consumer that is also `WantedBy=braid-online.service` (the recommended setup -- see the sharing guide).

Standalone CLI installs (no NixOS module) skip all three -- there is no `braid-online.service` or scrub unit to stop.

## Error handling

- If unmount fails after 3 retry attempts (e.g. a process has files open on the mount), lock still attempts to close the LUKS mappers and reports the failure
- If a mapper close fails with "device busy" after unmount also failed, the error is downgraded to a warning (the root cause is likely the stuck unmount)
- The hint `lsof <mount_point>` or `fuser -vm <mount_point>` is printed when unmount fails, to help identify the blocking process

## Related commands

- [braid unlock](unlock.md) -- open LUKS devices and mount the pool
- [braid status](status.md) -- check pool state before locking
- [braid idle](idle.md) -- check if operations are running before locking
