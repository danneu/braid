[← braid](../index.md)

# braid replace

Replace a disk with a new one using `btrfs replace`. Works for both live (still-online) and dead/missing disks.

## When to use it

- A disk has failed and you need to rebuild data onto a replacement
- Proactively swapping a healthy disk for a larger or newer one

## Basic example

The same invocation replaces a disk whether it is still live or already
dead/missing. braid resolves `--old` against `pool.json` to find the member and
detects its state automatically, so there is no mode to choose and `--missing-id`
is never required:

```
sudo braid replace --old toshiba1 --new toshiba4=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_NEW1
```

## Common variations

Note: `braid replace` operates only on btrfs-authoritative `MISSING` devids. A
drive that is hot-unplugged while the pool is mounted contributes to
`missing_count` and appears in `missing_devids` in `braid status` before btrfs
promotes its devid to `MISSING`; both an explicit `--missing-id` cross-check
and the no-flag auto-resolve path refuse the devid with a specific hot-unplug
diagnostic until that promotion happens. See
[Hot-unplug while pool is mounted](../guides/recovery-scenarios.md#hot-unplug-while-pool-is-mounted).

Optionally assert which missing devid you expect (braid refuses if it disagrees with pool.json):

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

Enroll a keyfile from a mounted USB drive on the new disk:

```
sudo braid replace \
  --old toshiba1 \
  --new toshiba4=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_NEW1 \
  --enroll /mnt/usb
```

Mount the USB first so the `--enroll` directory refers to removable media,
not persistent host storage.

Pass passphrase non-interactively:

```
sudo braid replace --old toshiba1 --new toshiba4=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_NEW1 --passphrase-file /tmp/pass.txt
```

## Important flags

| Flag | Purpose |
|---|---|
| `--old <name>` | Name of the disk to replace |
| `--new <name>=<path>` | Name and by-id path of the replacement disk |
| `--missing-id <devid>` | Optional cross-check for a dead-disk replace: assert the missing btrfs devid. braid refuses if it disagrees with the devid pool.json records for --old. Never required. |
| `--enroll <dir>` | Enroll `braid.key` from this directory into LUKS slot 1 on the new disk |
| `--dry-run` | Show what would happen without executing |
| `--yes` | Skip interactive confirmation |
| `--passphrase-stdin` | Read passphrase from stdin |
| `--passphrase-file <path>` | Read passphrase from a file (conflicts with `--passphrase-stdin`) |
| `--luks-format-arg=<ARG>` | Advanced: pass one raw argument to `cryptsetup luksFormat`, repeated once per argument; always use the equals form (e.g. `--luks-format-arg=--pbkdf`). braid refuses flags it manages itself -- identity, key-material, integrity, and on-disk-layout options such as `--uuid`, `--label`, `--type`, `--key-file`, and offset/sizing flags. |
| `--progress auto\|always\|never` | Control progress display (default: auto) |

## What happens under the hood

**For a fresh replacement disk (no LUKS):**

1. Pre-generates the replacement member's LUKS UUID and LUKS-formats the new disk with the pool passphrase and a `braid-<name>` label
2. Optionally enrolls a keyfile in slot 1
3. Creates a LUKS header backup
4. Opens the LUKS mapper

**Then, for all replacements:**

5. Runs `btrfs replace start` to copy data from the old device (or its mirrors) to the new device
6. Writes committed UUID-keyed membership to `pool.json` and advances the journal to post-replace maintenance
7. For live replacements: closes the old disk's LUKS mapper
8. Resizes the new device to use its full capacity (important when the new disk is larger)
9. For missing-disk replacements that clear the last missing device: runs a soft RAID1 balance to restore redundancy on any single-profile chunks
10. Clears the journal

The fresh-disk path always produces a local LUKS header backup in step 3; the existing-LUKS path produces one only when `--enroll` actually adds slot 1, so an already-enrolled disk is a no-op with no new backup. See [Pending LUKS header backups](status.md#pending-luks-header-backups) -- copy each `.luksheader` off-system and delete the local copy.

A sleep inhibitor is held throughout the replace to prevent the system from suspending. Suspending mid-replace can corrupt the btrfs topology.

If a btrfs exclusive operation (a running balance, device add/remove/replace, resize, or swap activate) is already in flight on the pool, braid does not fail -- its `btrfs` commands queue behind the in-flight operation (via `--enqueue`) and the kernel runs them when the pool is free. A *paused* balance is the exception and is refused (see Safety checks below).

## Safety checks / refusal cases

- Refuses if the pool is not mounted
- Refuses if `--old` and `--new` are the same disk
- Refuses if the new disk's LUKS UUID is already in use by the pool (registered membership or live btrfs devices) -- detach the conflicting disk before retrying
- Refuses if the new disk is absent (not plugged in)
- Refuses if the new disk's mapper capacity is smaller than the source disk's btrfs `total_bytes` (read via `BTRFS_IOC_DEV_INFO`, the same value `btrfs replace start` compares against). For existing LUKS targets, mapper capacity is derived from the LUKS2 segment `offset` and `size` (`dynamic` means `raw - offset`, fixed means the segment size). For fresh-LUKS targets, braid uses cryptsetup's default 16 MiB offset; offset-affecting `--luks-format-arg` flags (`--offset`/`-o`, `--align-payload`, `--luks2-metadata-size`, `--luks2-keyslots-size`, `--sector-size`) are rejected for this reason.
- For live replacements: refuses if the pool has missing devices (resolve those first)
- For missing replacements: refuses if `--missing-id` points to a live device
- For missing replacements: refuses if `--missing-id` disagrees with the devid pool.json records for `--old` (`--old` already identifies which member to rebuild)
- For missing replacements: refuses if pool.json has no recorded devid for `--old` -- `--missing-id` cannot substitute, it must match the recorded devid
- Verifies the passphrase against an existing pool member before formatting
- Warns before confirmation and in `--dry-run` if the live source device has I/O errors (informational, does not block)
- Warns if existing pool drives have a keyfile but `--enroll` was not passed
- Refuses if a pending operation journal (`pending-op.json`) exists -- run `braid recover` to reconcile.
- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
- Refuses if a btrfs balance is *paused* on the pool -- resume or cancel it first. A paused balance holds the exclusive-operation lock indefinitely, so braid cannot wait it out.
- Refuses when UPS support is enabled and `braid ups status` cannot verify a trusted `OL` (utility-power) state.

## Related commands

- [braid status](status.md) -- find device IDs and see which disks are missing
- [braid remove-missing](remove-missing.md) -- forget a dead device without replacing it
- [braid add](add.md) -- add a new disk (without replacing an existing one)
