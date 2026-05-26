[← braid](../index.md)

# braid add

Add one or more disks to the braid pool. Creates a new pool if none exists, or expands an existing one.

## When to use it

- Setting up a new NAS (bootstrap with one or more disks)
- Expanding storage by adding a new drive to an existing pool

## Basic example

```
sudo braid add toshiba1=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234
```

## Common variations

Bootstrap a new pool with two disks (creates RAID1 immediately):

```
sudo braid add \
  toshiba1=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234 \
  toshiba2=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_5678
```

Add a disk to an existing pool:

```
sudo braid add toshiba3=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_9012
```

Preview what would happen without making changes:

```
sudo braid add toshiba1=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234 --dry-run
```

Skip the confirmation prompt (for scripting):

```
sudo braid add toshiba1=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234 --yes
```

Pass passphrase non-interactively:

```
echo -n 'hunter2' | sudo braid add toshiba1=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234 --passphrase-stdin
sudo braid add toshiba1=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234 --passphrase-file /tmp/pass.txt
```

Enroll a keyfile for auto-unlock from a mounted USB drive at the same time:

```
sudo braid add toshiba1=/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234 --enroll /mnt/usb
```

Mount the USB first so the `--enroll` directory refers to removable media,
not persistent host storage.

## Important flags

| Flag | Purpose |
|---|---|
| `--dry-run` | Show what would happen without executing |
| `--yes` | Skip interactive confirmation |
| `--passphrase-stdin` | Read passphrase from stdin instead of TTY prompt |
| `--passphrase-file <path>` | Read passphrase from a file (conflicts with `--passphrase-stdin`) |
| `--enroll <dir>` | Enroll `braid.key` from this directory into LUKS slot 1 on new disks |
| `--progress auto\|on\|off` | Control progress display (default: auto) |

## Disk spec format

Each disk is specified as `NAME=PATH`, where:

- **NAME** is a short label you choose (e.g. `toshiba1`)
- **PATH** is the `/dev/disk/by-id/` stable device path

The name is stored in pool.json and used in LUKS mapper names (`braid-toshiba1`), LUKS labels, and all future commands. The persistent member identity is the LUKS UUID, not the name.

## What happens under the hood

1. Probes each disk to determine its state (fresh, braid-labeled, or foreign)
2. Shows a confirmation prompt with disk model, serial, and size
3. For fresh disks: pre-generates a LUKS UUID, LUKS-formats the disk with the pool passphrase and `braid-<name>` label, creates a LUKS header backup, and opens the LUKS mapper
4. If no pool exists: creates a btrfs filesystem (RAID1 if 2+ disks, single if 1 disk; braid explicitly pins the `block-group-tree` feature bit so that bit is visible and stable across toolchain versions -- see [ADR-027](../design/decisions/027-mkfs-block-group-tree.md))
5. If a pool exists: writes a phased UUID-keyed journal, adds the device to the existing btrfs filesystem, records the new membership in pool.json, then advances the journal to the balance phase
6. If the pool now has 2+ disks: balances data to RAID1, then clears the journal

A sleep inhibitor is held during all irreversible operations to prevent the system from suspending mid-operation.

If a btrfs exclusive operation (a running balance, device add/remove/replace, resize, or swap activate) is already in flight on the pool, braid does not fail -- its `btrfs` commands queue behind the in-flight operation (via `--enqueue`) and the kernel runs them when the pool is free. A *paused* balance is the exception and is refused (see Safety checks below).

## Disk acceptance rules

braid classifies each disk before acting:

- **Fresh disk** (no LUKS): accepted. LUKS-formatted with the pool passphrase.
- **Returning braid disk** (braid-labeled LUKS, btrfs FSID matches the pool): accepted as a recovery add. The disk is re-joined to the pool without reformatting. If the old btrfs signature would make `btrfs device add` refuse the disk, braid first runs the narrow wipe `wipefs --all --types btrfs` on the verified mapper, then uses `btrfs device add -f`.
- **Non-braid LUKS**: refused. braid will not adopt a LUKS device it did not create.
- **Braid-labeled, wrong pool**: refused. The disk belongs to a different btrfs filesystem.
- **Braid-labeled, no btrfs superblock**: refused. The disk's identity is ambiguous (could be partial init, clean eviction, or manual wipe). Wipe the disk and add as fresh.
- **Braid-labeled, pool not mounted**: refused during bootstrap. Identity cannot be verified without a mounted pool.

## Safety checks / refusal cases

- Rejects duplicate disk names in the same command
- Rejects disks that conflict with existing pool membership (same LUKS UUID, same name, or same by-id path)
- Rejects absent disks (not plugged in)
- Verifies the passphrase against an existing pool member before formatting new disks
- Warns if the pool has missing devices but does not refuse: `braid add` still attempts to add the new disk. It does not remove or replace the missing member, so even if the add succeeds the pool stays degraded. Run `braid replace` first to repair the missing member and return the pool to full health.
- Warns if existing pool drives have a keyfile but `--enroll` was not passed
- Refuses if a pending operation journal (`pending-op.json`) exists -- run `braid recover` to reconcile.
- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
- Refuses if a btrfs balance is *paused* on the pool -- resume or cancel it first. A paused balance holds the exclusive-operation lock indefinitely, so braid cannot wait it out.

## Interrupted adds

Existing-pool adds recover in two phases:

- **PoolMutation**: disk preparation and btrfs membership are not fully committed yet. `braid recover` may finish formatting a fresh target, re-open a verified returned target, run the narrow btrfs-signature wipe for that returned target, and run `btrfs device add`.
- **PostAddBalanceRaid1**: membership and `pool.json` are committed. `braid recover` will not format, wipe, or add disks in this phase; it only mounts/probes the committed pool, repairs `pool.json` from the committed live topology if needed, resumes or runs the owed RAID1 balance, and clears `pending-op.json`.

## Related commands

- [braid status](status.md) -- check pool health and disk info
- [braid remove](remove.md) -- remove a live disk from the pool
- [braid replace](replace.md) -- replace a dead or live disk
- [braid unlock](unlock.md) -- open LUKS devices and mount the pool

## Related guides

- [Getting started](../guides/getting-started.md) -- initial setup walkthrough
