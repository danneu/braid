[← Manual](../index.md)

# braid replace

Replace a disk with a new one using `btrfs replace`. Works for both live (still-online) and dead/missing disks.

## When to use it

- A disk has failed and you need to rebuild data onto a replacement
- Proactively swapping a healthy disk for a larger or newer one

## Basic example

Replace a live disk:

```
sudo braid replace --old toshiba1 --new toshiba4=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_NEW1
```

Replace a dead/missing disk (auto-detects devid when only one device is missing):

```
sudo braid replace --old toshiba1 --new toshiba4=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_NEW1
```

## Common variations

Replace a dead disk when multiple devices are missing (must specify which):

```
sudo braid replace \
  --old toshiba1 \
  --new toshiba4=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_NEW1 \
  --missing-id 3
```

Preview what would happen:

```
sudo braid replace --old toshiba1 --new toshiba4=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_NEW1 --dry-run
```

Enroll a keyfile on the new disk:

```
sudo braid replace \
  --old toshiba1 \
  --new toshiba4=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_NEW1 \
  --enroll /etc/braid/keys
```

Pass passphrase non-interactively:

```
sudo braid replace --old toshiba1 --new toshiba4=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_NEW1 --passphrase-file /tmp/pass.txt
```

## Important flags

| Flag | Purpose |
|---|---|
| `--old <name>` | Name of the disk to replace |
| `--new <name>=<path>` | Name and by-id path of the replacement disk |
| `--missing-id <devid>` | Target a specific missing device by btrfs devid (required when multiple devices are missing) |
| `--enroll <dir>` | Enroll `braid.key` from this directory into LUKS slot 1 on the new disk |
| `--dry-run` | Show what would happen without executing |
| `--yes` | Skip interactive confirmation |
| `--passphrase-stdin` | Read passphrase from stdin |
| `--passphrase-file <path>` | Read passphrase from a file |
| `--progress auto\|on\|off` | Control progress display (default: auto) |

## What happens under the hood

**For a fresh replacement disk (no LUKS):**

1. LUKS-formats the new disk with the pool passphrase and a `braid-<name>` label
2. Creates a LUKS header backup
3. Opens the LUKS mapper
4. Optionally enrolls a keyfile in slot 1

**Then, for all replacements:**

5. Runs `btrfs replace start` to copy data from the old device (or its mirrors) to the new device
6. After replace completes, resizes the new device to use its full capacity (important when the new disk is larger)
7. For live replacements: closes the old disk's LUKS mapper
8. For missing-disk replacements that clear the last missing device: runs a soft RAID1 balance to restore redundancy on any single-profile chunks
9. Updates pool.json with the new disk's membership info

A sleep inhibitor is held throughout the replace to prevent the system from suspending. Suspending mid-replace can corrupt the btrfs topology.

## Safety checks / refusal cases

- Refuses if the pool is not mounted
- Refuses if `--old` and `--new` are the same disk
- Refuses if the new disk is already a member of the pool
- Refuses if the new disk is absent (not plugged in)
- For live replacements: refuses if the pool has missing devices (resolve those first)
- For missing replacements: refuses if `--missing-id` points to a live device
- When multiple devices are missing: requires `--missing-id` to disambiguate
- Verifies the passphrase against an existing pool member before formatting
- Warns if the source device has I/O errors (informational, does not block)
- Warns if existing pool drives have a keyfile but `--enroll` was not passed
- Refuses if another braid operation is pending
- Refuses if a btrfs exclusive operation is already running

## Related commands

- [braid status](status.md) -- find device IDs and see which disks are missing
- [braid remove-missing](remove-missing.md) -- forget a dead device without replacing it
- [braid add](add.md) -- add a new disk (without replacing an existing one)
