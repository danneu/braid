---
experimental: false
---
[← braid](../index.md)

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
| `--passphrase-file <path>` | Read passphrase from a file (conflicts with `--passphrase-stdin`)                   |
| `--key-file <path>`        | Unlock with a binary keyfile instead of passphrase (conflicts with passphrase flags) |
| `--allow-degraded`         | Allow mounting with missing devices (degraded mode)                                  |
| `--dry-run`                | Show what would happen without executing                                             |

## What happens under the hood

1. Checks that no other braid operation is pending
2. Checks the mount point. If this pool is already mounted, unlock stops here and exits 0 (see below)
3. Probes each UUID-keyed member in pool.json: checks whether the by-id device is present, whether its LUKS UUID matches, and whether its LUKS mapper is already open
4. Verifies the selected credential against every disk it will unlock before opening any mapper
5. Opens LUKS mappers for all locked disks using the verified credential
6. Runs `btrfs device scan` to let the kernel discover all pool members
7. Mounts the btrfs filesystem with `noatime`, `skip_balance`, `subvolid=5`, and `nosuid,nodev` (privilege containment)
8. If any disks are unavailable and `--allow-degraded` is set: mounts with the `degraded` option
9. After mount: enriches pool.json with live btrfs device IDs and related metadata -- best-effort
10. Checks for a paused balance and prints a warning if one is found

If this pool is already mounted, unlock skips the disk probe, the passphrase prompt, and the mount, and exits 0. Mapper state does not matter on that path. "This pool" means a btrfs whose device is the LUKS mapper of a disk in `pool.json`. Unlock refuses any other mount at the mount point; see [Safety checks](#safety-checks--refusal-cases). A real run on a mounted pool can still change state, because the online finalizer runs after it.

### Online finalizer

On a real run, braid runs an online finalizer after unlock. [ADR 026](../design/decisions/026-pool-lock-rust-owned.md#decision) owns the rule for when it runs. Neither `--dry-run` nor an exit before unlock itself starts reaches it; the ADR lists those exits.

The finalizer acts only when this pool is mounted at the mount point, whether this run mounted it or it was mounted already:

- If the config sets a pool access group (`braid.poolAccessGroup` on module installs), it re-applies the mount root's ownership and mode; see [Mount point permissions](../guides/sharing-and-permissions.md#mount-point-permissions).
- On NixOS module installs, it starts `braid-online.service` if that unit is inactive or failed. Any unit configured through `braid.poolBoundServices` (or otherwise wired into the pool lifecycle with `WantedBy=braid-online.service`) starts as part of that activation. `braid lock` stops them again on the way down via the matching `BindsTo=braid-online.service`; see [Sharing and permissions](../guides/sharing-and-permissions.md).

So re-running `braid unlock` on a mounted pool restarts `braid-online.service`, and the units bound to it, after that unit went inactive or failed.

Standalone CLI installs (no NixOS module) have no `braid-online.service` to start. The finalizer still re-applies the mount root's ownership and mode when the config sets a pool access group.

## Degraded mode

When a disk is missing (physically absent or with an unreadable LUKS header), unlock refuses to mount by default. The error message names the affected disk and tells you to pass `--allow-degraded`.

In degraded mode, the pool mounts with reduced redundancy. New writes are NOT mirrored to the missing disk. You should repair the pool as soon as possible with `braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>`.

Unlock exits **2** when it refuses a degraded mount; see [Exit codes](#exit-codes).

## Exit codes

| Exit code | Meaning |
|---|---|
| **0** | Success: the pool is mounted, by this run or before it, or `--dry-run` printed its plan |
| **1** | Any other error |
| **2** | Degraded mount refused: a member is missing and `--allow-degraded` was not passed |

If every member is missing (absent or with an unreadable LUKS header), unlock exits 1 with `no unlockable disks found`, with or without `--allow-degraded`. There is nothing to mount, degraded or not.

Argument errors -- an unknown flag, a missing value, or conflicting flags such as `--key-file` with `--passphrase-stdin` -- also exit 2. clap prints its own diagnostic for them and never the `refusing to mount degraded` text a degraded refusal prints. Read stderr to tell the two exit-2 cases apart.

`braid-auto-unlock.service` always passes valid arguments, so its only exit 2 is a degraded refusal, which it logs as missing devices. It logs any other failure, including no unlockable disks, as `unlock failed (exit <code>), skipping`.

## Safety checks / refusal cases

- Refuses if a pending operation journal (`pending-op.json`) exists -- run `braid recover` to reconcile.
- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
- Refuses to mount degraded without explicit `--allow-degraded`. The flag covers only members that are absent or whose LUKS header cannot be read.
- Refuses (exit 1) if something other than this pool is mounted at the mount point. Unlock does not mount over it, and it does not change its ownership or start `braid-online.service` on it.
  - A filesystem that is not btrfs: `error: /mnt/storage is mounted but fstype is ext4, not btrfs`.
  - A btrfs that is provably not this pool: its device is not a LUKS mapper, or its LUKS UUID belongs to no disk in `pool.json`. The error names the device and the reason.
  - A mount braid cannot identify: `error: cannot tell whether the filesystem at /mnt/storage is this pool: <reason>`. The reason is, for example, an unreadable device-mapper entry, or a mount source braid cannot resolve, such as a `/dev/disk/by-id/...` link or btrfs's `<missing disk>`.

  For the first two, unmount it yourself (`sudo umount /mnt/storage`); `braid lock` refuses them too. For a mount braid cannot identify, `sudo braid lock` unmounts it as the pool, or unmount it yourself. Then re-run `braid unlock`.
- Refuses (exit 1) on anything else wrong with a member. `--allow-degraded` does not bypass it.
  - A problem found while probing stops unlock before any mapper opens. Examples: a present disk whose LUKS UUID does not match the UUID recorded in `pool.json`, a LUKS1 header, or a `braid-<name>` mapper already open on a different disk.
  - A failure while opening can come after earlier mappers opened. Examples: `cryptsetup open` fails on a later disk, or a mapper changed owner after the probe. Unlock then closes the mappers this run opened, as it does when the mount fails. Mappers that were open before unlock ran stay open. The close is best-effort: a close that fails prints a failure line for that disk.

  A LUKS UUID mismatch means the disk was swapped, cloned, or reformatted out of band. The error names the disk, its by-id path, and the expected vs found UUID. If unintended, detach the foreign disk and reattach the original; if the swap was intentional, see [Unlock refused by a foreign or mismatched disk](../guides/recovery-scenarios.md#unlock-refused-by-a-foreign-or-mismatched-disk).
- If any disk rejects the selected credential during verification, unlock fails before opening any mapper and names the failing disk. If another disk already accepted the same credential, that points to disk-specific credential drift outside braid.
- Does not prompt for a passphrase if all mappers are already open (idempotent re-run)

## Related commands

- [braid lock](lock.md) -- unmount the pool and close LUKS mappers
- [braid status](status.md) -- check pool health after unlocking
- [braid replace](replace.md) -- repair a missing disk after degraded unlock
