---
experimental: false
---
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
| `--enroll <dir>` | Enroll `braid.key` from this directory into LUKS slot 1 on each adopted disk -- fresh or returning -- whose slot 1 is empty; idempotent skip if the keyfile already authenticates slot 1 |
| `--luks-format-arg=<ARG>` | Advanced: pass one raw argument to `cryptsetup luksFormat`, repeated once per argument; always use the equals form (e.g. `--luks-format-arg=--pbkdf --luks-format-arg=argon2id`). braid refuses flags it manages itself -- identity, key-material, integrity, hardware-encryption (OPAL), and on-disk-layout options such as `--uuid`, `--label`, `--type`, `--key-file`, and offset/sizing flags. It also refuses a bare `--`, which would make cryptsetup ignore every argument after it, and options that make cryptsetup exit without formatting (`--help`, `--usage`, `--version`, `--test-args`, `-?`, `-V`). braid passes the disk to cryptsetup ahead of every `--luks-format-arg`, so no argument can change which disk is formatted; cryptsetup ignores a stray value that no option takes. Before it writes `pending-op.json`, braid runs the full `luksFormat` command with `--test-args`, which checks the options and writes nothing, and refuses with cryptsetup's message if cryptsetup rejects them. |
| `--progress auto\|always\|never` | Control progress display (default: auto) |

## Disk spec format

Each disk is specified as `NAME=PATH`, where:

- **NAME** is a short label you choose (e.g. `toshiba1`)
- **PATH** is a `/dev/disk/by-id/` link to a whole disk, not a partition. A disk has several links (`ata-`, `wwn-`, ...); pass one of them, once.

The name is stored in pool.json and used in LUKS mapper names (`braid-toshiba1`), LUKS labels, and all future commands. The persistent member identity is the LUKS UUID, not the name.

## What happens under the hood

1. Probes each disk to determine its state (fresh, braid-labeled, or foreign)
2. Shows a confirmation prompt with the disk's name and by-id path, plus its model/size/serial (best-effort from the live device via lsblk -- omitted if unavailable). This interactive prompt is skipped by `--yes` and not shown in `--dry-run`.
3. For fresh disks: pre-generates a LUKS UUID, LUKS-formats the disk with the pool passphrase and `braid-<name>` label, enrolls the `--enroll` keyfile into slot 1 if provided, creates a LUKS header backup, and opens the LUKS mapper

   See [Pending LUKS header backups](status.md#pending-luks-header-backups) -- copy each `.luksheader` off-system and delete the local copy.
4. If no pool exists: creates a btrfs filesystem (RAID1 if 2+ disks, single if 1 disk; braid explicitly pins the `block-group-tree` feature bit so that bit is visible and stable across toolchain versions -- see [ADR-027](../design/decisions/027-mkfs-block-group-tree.md))
5. If a pool exists: writes a phased UUID-keyed journal, adds the device to the existing btrfs filesystem, records the new membership in pool.json, then advances the journal to the balance phase
6. If the pool now has 2+ disks: balances data to RAID1, then clears the journal -- **unless the pool has a missing device, in which case the balance is skipped** (a `[skip]` note explains why). Redundancy is restored later by [`remove-missing`](remove-missing.md) or [`replace`](replace.md), not by the degraded add.

**Keyfile enrollment (`--enroll DIR`):** braid enrolls `braid.key` into LUKS slot 1 on every adopted disk -- fresh or returning. On a fresh disk slot 1 is always empty, so the keyfile is always added. On a returning braid disk braid first probes the keyfile: if it already authenticates slot 1 the enrollment is an idempotent skip with no slot change and no new header backup; if slot 1 is empty the keyfile is added. (If slot 1 holds a different, unknown key braid refuses -- see Safety checks.) The keyfile is added before the header backup so the backup captures slot 1.

A sleep inhibitor is held during all irreversible operations to prevent the system from suspending mid-operation.

If a btrfs exclusive operation (a running balance, device add/remove/replace, resize, or swap activate) is already in flight on the pool, braid does not fail -- its `btrfs` commands queue behind the in-flight operation (via `--enqueue`) and the kernel runs them when the pool is free. A *paused* balance is the exception and is refused (see Safety checks below).

## Disk acceptance rules

braid classifies each disk before acting:

- **Fresh disk** (no LUKS): accepted. LUKS-formatted with the pool passphrase.
- **Returning braid disk** (not in `pool.json`, braid-labeled LUKS, btrfs FSID matches the pool): accepted as a recovery add. The disk is re-joined to the pool without reformatting. If the old btrfs signature would make `btrfs device add` refuse the disk, braid first runs the narrow wipe `wipefs --all --types btrfs` on the verified mapper, then uses `btrfs device add -f`.
- **Non-braid LUKS**: refused. braid will not adopt a LUKS device it did not create.
- **Braid-labeled, wrong pool**: refused. The disk belongs to a different btrfs filesystem.
- **Braid-labeled, no btrfs superblock**: refused. The disk's identity is ambiguous (could be partial init, clean eviction, or manual wipe). Wipe the disk and add as fresh.
- **Braid-labeled, pool not mounted**: refused during bootstrap. Identity cannot be verified without a mounted pool.

## Safety checks / refusal cases

- Rejects duplicate disk names in the same command
- Rejects two disks that are the same disk: one by-id link passed twice, or two links (such as `ata-...` and `wwn-...`) that point to one device. Pass each disk once.
- Rejects a disk that is not a whole disk. braid asks the kernel (sysfs), not the link's name, so it refuses a partition even when its link has no `-partN` suffix, and refuses a device sysfs cannot answer for. Pass the disk's own by-id link. These checks run when planning, so `--dry-run` refuses too.
- Rejects a name that a pool member already uses with a different by-id path.
- Rejects a by-id path that a pool member already uses under a different name.
- Rejects a disk whose LUKS UUID matches another disk in the same command, a `pool.json` member listed under another name and by-id, or a device already live in the pool. This usually means a `dd`-cloned disk is attached; detach the cloned or unintended disk and retry.
- A disk that `pool.json` already lists under the same name and by-id path is that member, not a name or by-id conflict. If the member is live in the mounted pool on the same disk, add skips it, and confirms and adds only the other disks. When add skips every disk you named, it prints `Nothing to do -- <names> already in pool.` and writes no journal. Any other such member refuses the whole command, with the next step for its state:
  - Its LUKS mapper is open but its disk is gone (hot-unplugged): the reopen route [`braid remove`](remove.md) also gives. Run `braid lock` then `braid unlock` if the disk is back, or `braid lock` then `braid unlock --allow-degraded` if it is gone.
  - No open mapper in the pool holds it, but its disk is attached with the member's LUKS UUID, such as a disk that came back after a degraded unlock. btrfs does not take a returning disk back into a mounted pool, so run `braid lock` then `braid unlock`, then [`braid doctor`](doctor.md).
  - Its disk is not present. If the disk is dead, replace the member with [`braid replace`](replace.md). Otherwise plug it in, then run `braid lock`, `braid unlock` and `braid doctor`.
  - Its disk is attached but is not LUKS, or carries another LUKS UUID. `braid unlock` cannot open the member from it, so replace the member with `braid replace`.
  - Another disk in the pool carries its LUKS UUID: refused as a duplicate LUKS UUID, as above.
  - Its by-id link is gone while the member is live: refused as not present.

  The reopen steps add `--allow-degraded` while another member is missing. A skipped member gets no label or `--enroll` check; [`braid enroll`](enroll.md) enrolls a keyfile on existing members. These checks run when planning, so `--dry-run` refuses too.
- Refuses when something other than btrfs is mounted at the pool mount point (`braid.mountPoint`): `<mount point> is already mounted with <fstype>, not btrfs. Unmount it first.` Unmount it, then re-run. This check runs when planning, so `--dry-run` refuses too.
- Refuses when `pool.json` lists members but the pool is not unlocked (nothing is mounted at the pool mount point), so add never mistakes a locked pool for a new one. Run `braid unlock` first, with `--allow-degraded` if a member is missing, then re-run `braid add`; the missing-devices bullet below covers the repair. If `pool.json` is stale, back it up and move it aside, attach every intended member, then run [`braid discover --write`](discover.md) to rebuild it. The rebuild leaves out any member that is not attached. This check runs when planning, so `--dry-run` refuses too.
- Refuses when a pool is mounted at the pool mount point but `pool.json` is missing or lists no members, so add never writes a `pool.json` that lists only the new disks. Attach every intended member, run [`braid discover --write`](discover.md) to rebuild `pool.json`, then re-run `braid add`. If an empty `pool.json` exists, back it up and move it aside first, because `discover --write` refuses any valid `pool.json`. This check runs when planning, so `--dry-run` refuses too.
- Refuses when the mounted pool holds an open LUKS mapper whose LUKS UUID `pool.json` does not list, whether its disk is present or gone. The error names each such mapper and its UUID. Either the mount is not the braid pool, or `pool.json` is stale; [`braid doctor`](doctor.md) helps tell which. For a stale `pool.json`, back it up and move it aside, attach every intended member, then run `braid discover --write`. This refusal also covers naming a live disk that `pool.json` leaves out, which does not print `Nothing to do`. A device btrfs reports as MISSING with no open mapper does not trigger it, so a degraded add still runs. This check runs when planning, so `--dry-run` refuses too, and again just before the journal.
- Rejects absent disks (not plugged in)
- Verifies the passphrase against an existing pool member before formatting new disks
- Refuses when cryptsetup rejects the `--luks-format-arg` options for a fresh disk. braid checks them with `cryptsetup luksFormat --test-args`, which writes nothing, when planning, so `--dry-run` refuses too. cryptsetup checks the options themselves, not whether the kernel supports a cipher or hash they name, so such an option can still fail the format after the journal; `braid recover` then drops that disk (see [Interrupted adds](#interrupted-adds)).
- Refuses a fresh disk that is not ready to format, both when planning (so `--dry-run` refuses too) and again just before the journal: if its `/dev/mapper/braid-<name>` mapper is already open, close it with `sudo cryptsetup close braid-<name>`; if a mount, an LVM volume group, an md array, or a device-mapper device holds the disk, free it (`lsblk <by-id path>` shows the holder). Then re-run.
- Warns if the pool has missing devices but does not refuse: `braid add` still adds the new disk, but **skips the RAID1 convert balance** (it surfaces a `[skip]` note), so the pool stays degraded and redundancy is not restored at add time. To repair, either run `braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>` to swap in a new disk for the missing member, or -- on a 2-disk degraded pool, where [`remove-missing`](remove-missing.md) alone would leave one disk with no redundancy -- run `braid add` then `braid remove-missing` to drop the dead member and rebalance onto the new disk.
- Warns if existing pool drives have a keyfile but `--enroll` was not passed
- With `--enroll`, refuses if an adopted disk's LUKS slot 1 is occupied by an unknown key the keyfile does not authenticate -- remove it first with `cryptsetup luksKillSlot`, then retry.
{{#include ../_includes/mutating-command-refusals.md.inc}}

## Interrupted adds

`braid recover` never formats a disk.

A bootstrap add creates the pool only when mkfs.btrfs runs, after it formats every disk. If it stopped before that, there is no pool to recover: `braid recover` clears the journal and lists each disk luksFormat wrote. Wipe those disks (`sudo wipefs -a <by-id path>`) before you re-run `braid add`, since add refuses a braid-labeled disk with no btrfs. With the pool not mounted, recover checks every journaled disk first: if one is gone, it refuses and keeps the journal until you reconnect it, even if mkfs.btrfs ran. [What happens under the hood](recover.md#what-happens-under-the-hood) has the details.

Existing-pool adds recover in two phases:

- **PoolMutation**: disk preparation and btrfs membership are not fully committed yet. For each journaled target that is not a live pool member yet, `braid recover` finishes the steps `braid add` planned for that kind of disk:
  - **Fresh disk:** if the disk is still not LUKS, luksFormat never wrote it, so btrfs never held it. Recover drops it from the journal; re-run `braid add` for it. If no target remains, recover writes the pre-operation `pool.json` and clears the journal. If the disk carries the `braid-<name>` label and the journaled LUKS UUID, recover enrolls the keyfile if the journal records one, backs up the LUKS header, opens the mapper, and runs `btrfs device add`.
  - **Returned disk:** recover opens the mapper if it is closed. If the mapper still shows a btrfs signature, recover checks that it carries this pool's FSID. If the journal records a keyfile, recover enrolls it and backs up the LUKS header. Then it runs the narrow btrfs-signature wipe and `btrfs device add -f`.

  See [Pending LUKS header backups](status.md#pending-luks-header-backups) -- copy each `.luksheader` off-system and delete the local copy. The [`braid recover` safety checks](recover.md#safety-checks) say when recover refuses a target and keeps the journal.
- **PostAddBalanceRaid1**: membership and `pool.json` are committed. `braid recover` will not format, wipe, or add disks in this phase; it only mounts/probes the committed pool, repairs `pool.json` from the committed live topology if needed, runs the owed RAID1 balance when btrfs balance state is idle and the pool can host RAID1 (no member missing or hot-unplugged, two or more present), and clears `pending-op.json` after that succeeds or skips. A paused, running, or unknown balance state fails closed with the journal preserved.

## Related commands

- [braid status](status.md) -- check pool health and disk info
- [braid remove](remove.md) -- remove a live disk from the pool
- [braid replace](replace.md) -- replace a dead or live disk
- [braid unlock](unlock.md) -- open LUKS devices and mount the pool

## Related guides

- [Getting started](../guides/getting-started.md) -- initial setup walkthrough
