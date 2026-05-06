[← Manual](../index.md)

# Recovery scenarios

Detailed walkthroughs for recovering from failures. Read this when `braid status` or another command tells you something is wrong, or when you are planning for failure ahead of time.

## Overview: discover vs recover

braid has two recovery commands that solve different problems:

| Command | When to use | What it does |
| --- | --- | --- |
| `braid discover --write` | pool.json is missing or corrupted | Scans disk labels to rebuild pool.json |
| `braid recover` | pending-op.json exists (interrupted mutation) | Opens pool, probes live topology, rebuilds pool.json, clears journal |

**discover** solves metadata loss -- the CLI's record of which disks belong to the pool is gone, but the disks themselves are fine. It reads LUKS labels (`braid-<name>`) from `/dev/disk/by-id/` devices to reconstruct membership.

**recover** solves interrupted operations -- an `add`, `remove`, `remove-missing`, or `replace` was killed mid-flight (power loss, crash, OOM). The pending-operation journal (`/var/lib/braid/pending-op.json`) records what was in progress. Recover opens the pool, inspects what actually happened on disk, and rebuilds pool.json to match reality.

## Lost pool.json

**Symptom:** `braid unlock` fails because `/var/lib/braid/pool.json` does not exist.

**Cause:** Accidental deletion, filesystem corruption, or migrating to a new NixOS install.

### Steps

1. Verify no pending operation exists:

```sh
ls /var/lib/braid/pending-op.json
# If this file exists, use `braid recover` instead (see below)
```

2. Scan for braid disks:

```sh
sudo braid discover
```

Output looks like:

```
  toshiba1 = /dev/disk/by-id/ata-TOSHIBA_MN08ACA16T_XXXX
  toshiba2 = /dev/disk/by-id/ata-TOSHIBA_MN08ACA16T_YYYY
  toshiba3 = /dev/disk/by-id/ata-TOSHIBA_MN08ACA16T_ZZZZ
```

3. Verify the output matches your expected pool members. Then write:

```sh
sudo braid discover --write
```

This creates `/var/lib/braid/pool.json`.

4. Unlock normally:

```sh
sudo braid unlock
```

### Notes

- `discover` refuses to run if pool.json already exists. Remove it first if it exists but is wrong.
- `discover` refuses to run if pending-op.json exists. Use `braid recover` instead.
- `discover` only finds LUKS2 devices. LUKS1 devices with braid labels are skipped with a warning.
- When multiple `/dev/disk/by-id/` symlinks point to the same device, discover picks the most stable one (wwn > nvme > scsi > ata > usb).

## Interrupted add/remove/replace

**Symptom:** braid commands fail with a message about a pending operation. `ls /var/lib/braid/pending-op.json` confirms the journal file exists.

**Cause:** A pool mutation (`add`, `remove`, `remove-missing`, `replace`) was interrupted before it could complete. The journal records the operation type, the pre-operation membership, and the target membership. Existing-pool `add` journals also record a phase: `PoolMutation` for unfinished disk preparation or btrfs membership, and `PostAddBalanceRaid1` after membership is committed but balance work remains.

### Steps

1. Preview what recover will do:

```sh
sudo braid recover --dry-run
```

This shows the recovery plan without making changes: which LUKS devices will be opened, whether the pool needs mounting, and the final pool.json state.

2. Run recovery:

```sh
sudo braid recover
```

Recover will:
- Open the LUKS devices needed for the journal phase
- Mount the btrfs pool
- Probe the live btrfs topology to determine what actually happened
- For existing-pool add `PoolMutation`, first open and scan any already-committed journaled add targets that can be reconciled without wiping or adding
- For add `PoolMutation`, finish only the journaled add targets that are not already live
- For add `PostAddBalanceRaid1`, skip all disk preparation and btrfs add steps, then finish the owed RAID1 balance
- Rebuild or repair pool.json only when live membership is complete
- Clear pending-op.json only after required membership and balance work is complete

3. Verify:

```sh
sudo braid status
```

### Interrupted between returned-disk wipe and add

If an existing braid-labeled disk was being returned to the pool and the add was interrupted after `wipefs --types btrfs` but before `btrfs device add`, run:

```sh
sudo braid recover
```

Recover replays the add from the journaled returned-disk target. Do not wipe the disk and retry it as a fresh add; the journal still records the checked LUKS identity and expected pool FSID.

### Interrupted fresh-disk add

For an interrupted fresh-disk add, recover replays the format, optional keyfile enrollment, LUKS header backup, mapper open, and `btrfs device add` from the journaled options when the disk is present.

If the disk is absent or has a different LUKS label than the journal records, recover fails and leaves `pending-op.json` in place. Reconnect the original disk or replace the target, then rerun `sudo braid recover`.

### Committed-but-closed add target

If the journaled add target is already a live pool member but its mapper is closed when recover starts, recover opens and scans it during the reconciliation pass. After the live-pool re-probe, the target is included in `pool.json` and is not re-added.

This can still prompt for the pool passphrase even when the pool is already mounted, because the target mapper may need to be opened before recover can discover that it already committed.

### With missing devices

If a drive failed during the interrupted operation:

```sh
sudo braid recover --allow-degraded
```

Without `--allow-degraded`, recover exits with code 2 when devices are missing. The degraded flag allows mounting with missing devices so recovery can complete. New writes will have no redundancy until the missing device is replaced.

### Scripted recovery

For unattended recovery (e.g. from a remote script):

```sh
echo "my-passphrase" | sudo braid recover --passphrase-stdin
```

Or with a passphrase file:

```sh
sudo braid recover --passphrase-file /path/to/passphrase
```

### Recover for a replace journal when the pool is already mounted

**Symptom:** `sudo braid recover` exits with `recover refuses to probe an already-mounted pool when the journal records a replace ...` and instructs you to run `braid lock` first.

**Cause:** The pool was mounted by something other than `braid recover` itself (typically a manual `cryptsetup open` + `mount` after a crash, since `braid unlock` and `braid-auto-unlock.service` both refuse to mount when a pending-op journal exists). For a replace journal, the kernel may have resumed an interrupted `dev_replace` on that mount session, leaving stale in-memory device state that recover cannot distinguish from real topology. The cycle that scrubs this state needs to unmount and remount, which is unsafe on a mount recover does not own.

#### Steps

```sh
sudo braid lock      # works with a journal present -- no pending-op preflight
sudo braid recover   # opens its own mount and runs the relock cycle
```

`braid lock` unmounts the pool and closes the LUKS mappers. `braid recover` then opens a fresh mount session, finishes any in-progress kernel `dev_replace`, and runs the umount-and-remount cycle that clears stale `btrfs_fs_devices` -- the standard happy path for replace recovery.

## Missing disk (drive failure)

**Symptom:** `braid status` shows a device as missing. The pool may be mounted degraded or may refuse to mount.

### Unlock with a missing disk

If the pool is not mounted:

```sh
sudo braid unlock --allow-degraded
```

This mounts the pool in degraded mode. All data is still accessible (btrfs RAID1 keeps a copy on the surviving disk(s)), but new writes have no redundancy until you replace the dead drive.

### Option A: Replace the disk

Replaces the dead disk with a new one, rebuilding data from surviving copies:

```sh
sudo braid replace --old toshiba2 \
  --new toshiba4=/dev/disk/by-id/ata-NEW_DRIVE_SERIAL
```

For a dead disk, replace may need the btrfs devid of the missing device. If prompted, find it from `braid status` and pass it:

```sh
sudo braid replace --old toshiba2 \
  --new toshiba4=/dev/disk/by-id/ata-NEW_DRIVE_SERIAL \
  --missing-id 3
```

Replace runs `btrfs replace` under the hood. This is a background operation that can take hours for large drives. Progress is visible in `braid status` and `braid tui`.

### Option B: Remove the missing device

Forgets the dead device without rebuilding data:

```sh
# Find the missing device's btrfs devid from braid status
sudo braid remove-missing --missing-id 3
```

Use this when you do not have a replacement disk. The pool continues with fewer disks and reduced capacity. Data that was only on the dead drive is lost (but in RAID1, all data has a second copy on another drive).

### Choosing between replace and remove-missing

| | `replace` | `remove-missing` |
| --- | --- | --- |
| Requires new disk | Yes | No |
| Rebuilds data | Yes | No |
| Restores redundancy | Yes | No |
| Duration | Hours (large disks) | Minutes |
| When to use | You have a replacement | No replacement available |

## Degraded mount

A degraded mount means at least one pool disk is missing. The pool is usable but new writes have no redundancy on the missing device's share of data.

### When degraded mounts happen

- `braid unlock --allow-degraded` -- explicit request
- `braid recover --allow-degraded` -- recovery with missing devices
- `braid.autoUnlock.allowDegraded = true` -- auto-unlock config

### Risks

- **No redundancy for new writes** -- data written while degraded exists on fewer disks. A second drive failure could lose data.
- **No self-healing** -- btrfs cannot repair corrupted blocks from a redundant copy if the copy was on the missing device.

### Resolution

Replace the missing disk as soon as possible:

```sh
sudo braid replace --old <dead-name> \
  --new <new-name>=/dev/disk/by-id/<new-drive>
```

After replace completes, the pool is fully redundant again.

## Recovery decision tree

```
braid command fails
├── "pending operation" error
│   └── braid recover [--allow-degraded]
├── pool.json missing
│   └── braid discover --write → braid unlock
├── missing device / won't mount
│   ├── braid unlock --allow-degraded
│   └── then: braid replace or braid remove-missing
└── something else
    └── braid doctor → check troubleshooting guide
```

## State files reference

All state lives under `/var/lib/braid/`:

| File | Purpose |
| --- | --- |
| `pool.json` | Pool membership (disk names and by-id paths) |
| `pending-op.json` | Pending operation journal (present only during mutations) |
| `acked-stats.json` | Acknowledged btrfs device stats baseline |
| `smartd-alert` | Flag file set by smartd alert script |
| `alert-latch.json` | Active alert state |
| `luks-headers/` | LUKS header backups |

## Related

- [Troubleshooting](troubleshooting.md) -- symptom-oriented quick fixes
- [NixOS configuration](nixos-configuration.md) -- `autoUnlock.allowDegraded` and other options
