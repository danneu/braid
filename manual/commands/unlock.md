[← Manual](../index.md)

# braid unlock

Open LUKS devices and mount the btrfs pool.

## When to use it

- After a boot, to unlock and mount the pool

Unless you've configured braid to automatically unlock on boot (`braid.autoUnlock`), you must use `braid unlock` to mount and access the pool.

## Basic example

```
sudo braid unlock
```

You will be prompted for the pool passphrase.

## Common variations

Pass passphrase non-interactively (useful for scripts or remote unlock):

```
echo -n 'hunter2' | sudo braid unlock --passphrase-stdin
sudo braid unlock --passphrase-file /tmp/pass.txt
```

Unlock with a binary keyfile from a mounted USB drive (e.g. for auto-unlock via systemd):

```
sudo braid unlock --key-file /mnt/usb/braid.key
```

Mount the USB first so the keyfile path refers to removable media, not
persistent host storage.

Mount in degraded mode when a disk is missing:

```
sudo braid unlock --allow-degraded
```

Preview what would happen:

```
sudo braid unlock --dry-run
```

## Important flags

| Flag                       | Purpose                                                                              |
| -------------------------- | ------------------------------------------------------------------------------------ |
| `--passphrase-stdin`       | Read passphrase from stdin instead of TTY prompt                                     |
| `--passphrase-file <path>` | Read passphrase from a file                                                          |
| `--key-file <path>`        | Unlock with a binary keyfile instead of passphrase (conflicts with passphrase flags) |
| `--allow-degraded`         | Allow mounting with missing devices (degraded mode)                                  |
| `--dry-run`                | Show what would happen without executing                                             |

## What happens under the hood

1. Checks that no other braid operation is pending
2. Probes each UUID-keyed member in pool.json: checks whether the by-id device is present, whether its LUKS UUID matches, and whether its LUKS mapper is already open
3. Verifies the passphrase against the first unlockable disk
4. Opens LUKS mappers for all locked disks using the verified passphrase
5. Runs `btrfs device scan` to let the kernel discover all pool members
6. Mounts the btrfs filesystem with `noatime`, `skip_balance`, and `subvolid=5`
7. If any disks are unavailable and `--allow-degraded` is set: mounts with the `degraded` option
8. After mount: enriches pool.json with live btrfs device IDs and related metadata -- best-effort
9. Checks for a paused balance and prints a warning if one is found

If all mappers are already open and the pool is already mounted, unlock is a no-op.

## Degraded mode

When a disk is missing (physically absent or with a damaged LUKS header), unlock refuses to mount by default. The error message names the affected disk and tells you to pass `--allow-degraded`.

In degraded mode, the pool mounts with reduced redundancy. New writes are NOT mirrored to the missing disk. You should repair the pool as soon as possible with `braid replace`.

The exit code is **2** when a degraded mount is refused (vs. **1** for other errors), so scripts can distinguish the two cases.

## Safety checks / refusal cases

- Refuses if another braid operation is pending
- Refuses to mount degraded without explicit `--allow-degraded`
- If a disk rejects the passphrase after another disk accepted it, the error names both disks (indicates someone changed a disk's passphrase outside braid)
- Does not prompt for a passphrase if all mappers are already open (idempotent re-run)

## Related commands

- [braid lock](lock.md) -- unmount the pool and close LUKS mappers
- [braid status](status.md) -- check pool health after unlocking
- [braid replace](replace.md) -- repair a missing disk after degraded unlock
