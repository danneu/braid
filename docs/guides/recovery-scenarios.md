[← braid](../index.md)

# Recovery scenarios

Detailed walkthroughs for recovering from failures. Read this when `braid status` or another command tells you something is wrong, or when you are planning for failure ahead of time.

## Overview: discover vs recover

braid has two recovery commands that solve different problems:

| Command | When to use | What it does |
| --- | --- | --- |
| `braid discover --write` | pool.json is missing or corrupted | Scans disk labels to rebuild pool.json |
| `braid recover` | pending-op.json exists (interrupted mutation) | Opens pool, probes live topology, rebuilds pool.json, and clears the journal after the idle/no-paused recovery path succeeds; preserves the journal when owed RAID1 replay finds a paused, running, or unknown balance state |

**discover** solves metadata loss -- the CLI's record of which disks belong to the pool is gone, but the disks themselves are fine. It reads LUKS labels (`braid-<name>`) and LUKS UUIDs from `/dev/disk/by-id/` devices to reconstruct UUID-keyed membership.

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

If you can name the expected member count ahead of time, record it from your
own records or prior `braid status` output and pass it as a fail-closed guard:

```sh
EXPECTED=3
sudo braid discover --write --expect-count="$EXPECTED"
```

4. Unlock normally:

```sh
sudo braid unlock
```

### Notes

- For a healthy UUID-keyed `pool.json`, `discover --write` refuses -- use `braid add` / `braid remove` / `braid replace` to mutate membership instead.
- For a corrupt or off-schema existing `pool.json`, `discover --write` rebuilds in place; no manual remove step is needed. The original bytes are preserved at `pool.json.corrupt-<RFC3339-UTC>` adjacent to the new file in case manual forensic recovery is needed (e.g. extracting a `devid` for a `null_underlying` member). The snapshot is a hard precondition: if it cannot be written (full disk, read-only state directory), `discover --write` refuses rather than destroy the corrupt original; free disk space or fix permissions and retry.
- `discover --write` refuses to run if `pending-op.json` exists. Use `braid recover` instead. (Bare `discover` is read-only and runs regardless.)
- `discover` only finds LUKS2 devices. LUKS1 devices with braid labels are skipped with a warning.
- The rebuilt `pool.json` is keyed by LUKS UUID. Disk names are stored in each member value for command input and display.
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
- For add `PostAddBalanceRaid1`, skip all disk preparation and btrfs add steps, then run the owed RAID1 balance only when btrfs balance state is idle; preserve `pending-op.json` when a paused, running, or unknown balance state requires manual inspection
- Rebuild or repair pool.json only when live membership is complete
- Clear pending-op.json only after required membership and balance work is complete

3. Verify:

```sh
sudo braid status
```

If recovery itself fails, its final guidance comes from a fresh authoritative
read of `pending-op.json`:

- A valid journal is still visible: address the reported failure, then rerun
  `sudo braid recover`. Recovery is designed to retry from the journaled phase.
- The journal is absent: there is no recovery state to retry. If recovery had
  just cleared the journal, deletion durability was not confirmed; repair
  state-directory I/O, restart, and run recovery only if `pending-op.json`
  reappears.
- The journal cannot be read or parsed: do not keep rerunning recovery. Repair
  state-directory access, or follow [Pending-op file corruption](#pending-op-file-corruption)
  for manual reconciliation of invalid contents.

### Interrupted between returned-disk wipe and add

If an existing braid-labeled disk was being returned to the pool and the add was interrupted after `wipefs --types btrfs` but before `btrfs device add`, run:

```sh
sudo braid recover
```

Recover replays the add from the journaled returned-disk target. Do not wipe the disk and retry it as a fresh add; the journal still records the checked LUKS identity and expected pool FSID.

### Interrupted fresh-disk add

For an interrupted fresh-disk add, recover replays the format, optional keyfile enrollment, LUKS header backup, mapper open, and `btrfs device add` from the journaled options when the disk is present.

If the disk is absent or has a different LUKS label than the journal records, recover fails and leaves `pending-op.json` in place. Reconnect the original disk or replace the target, then rerun `sudo braid recover`.

## Pending-op file corruption

**Symptom:** braid reports that `/var/lib/braid/pending-op.json` could not be parsed.

The remediation phrase is:

`Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/internals/luks-unlock.md) and re-run.`

It is safe to remove `pending-op.json` only when one of these is true:

| Situation | Safe? |
| --- | --- |
| No disk-level mutation committed: no LUKS format, no `btrfs device add`, no cryptsetup open of a fresh-format target | Yes |
| `braid status` confirms the live pool already reflects the intended state and the journal is stale | Yes |
| A mutation is partially complete, such as `mkfs.btrfs` run but `btrfs device add` did not, or `replace` is paused mid-rebuild | No |

When it is not safe, keep the journal in place and investigate the interrupted operation before editing state.

## Out-of-band reformat during recovery

Recover refuses if a target disk's live LUKS UUID no longer matches the journal. This catches a disk that was reformatted, swapped, or cloned after the original operation started.

Messages to search for:

- `add recovery aborted: target ... LUKS UUID mismatch`
- `recover replace target '...' LUKS UUID mismatch: expected ..., found ...`

Do not force the journal forward. Investigate the foreign reformat or swapped disk, restore the intended disk if possible, and rerun recovery.

See also [Unlock refused by a foreign or mismatched disk](#unlock-refused-by-a-foreign-or-mismatched-disk) for the same identity check on the `braid unlock` path.

## Never-enriched member with null-underlying mapper

A member can be known to btrfs by devid while its LUKS backing device is gone (`cryptsetup status` reports `device: (null)`). If the member was never enriched with a persisted devid, recovery cannot bind that null-underlying mapper back to a UUID-keyed membership entry.

Let `braid recover` complete when it can preserve the member. The next read-side command observes the live devid and `braid remove-missing` becomes available again if the device is truly gone.

## Duplicate or missing devid in journal snapshot

Recovery may refuse with internal errors equivalent to duplicate journaled devids or no member for a journaled devid. This means the journal snapshot cannot safely resolve a btrfs devid to a UUID-keyed member.

Do not edit `pool.json`; that resolution did not consult it. Re-run recovery only after manual reconciliation of `pending-op.json`.

### Committed-but-closed add target

If the journaled add target is already a live pool member but its mapper is closed when recover starts, recover opens and scans it during the reconciliation pass. After the live-pool re-probe, the target is included in `pool.json` and is not re-added.

This can still prompt for the pool passphrase even when the pool is already mounted, because the target mapper may need to be opened before recover can discover that it already committed.

### With missing devices

If a drive failed during the interrupted operation:

```sh
sudo braid recover --allow-degraded
```

Without `--allow-degraded`, recover exits with code 2 when devices are missing. The degraded flag allows mounting with missing devices so recovery can complete. Redundancy is reduced until the missing device is replaced.

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

## Unlock refused by a foreign or mismatched disk

**Symptom:** `braid unlock` exits with `LUKS UUID mismatch`. A disk at a recorded by-id slot reports a LUKS UUID that differs from the one in `pool.json`; the error names the disk, its by-id path, and the expected vs found UUID.

**Cause:** The disk was swapped, cloned, or reformatted out of band, so its LUKS identity no longer matches the recorded member. This is a hard refusal during probing, before any mapper opens. `--allow-degraded` does not bypass it -- that flag only covers *missing* disks, and this disk is present.

### If the swap was unintended

Detach the foreign disk and reattach the original. `braid unlock` then succeeds.

### If the swap was intentional

`braid replace` requires the pool mounted, but the present mismatched disk blocks the mount. Make the slot read as *missing* first, then replace:

1. Detach the foreign disk so the member reads as absent.
2. Mount the pool degraded:
   ```sh
   sudo braid unlock --allow-degraded
   ```
3. Replace the now-missing member following [Missing disk -> Option A: Replace the disk](#option-a-replace-the-disk). `braid replace` prepares its own `--new` disk; see [`braid replace`](../commands/replace.md) for how it handles a disk that already carries a LUKS header.

See also [Out-of-band reformat during recovery](#out-of-band-reformat-during-recovery) for the same identity check on the `braid recover` path (a different trigger).

## Missing disk (drive failure)

**Symptom:** `braid status` shows a device as missing. The pool may be mounted degraded or may refuse to mount.

### Unlock with a missing disk

If the pool is not mounted:

```sh
sudo braid unlock --allow-degraded
```

This mounts the pool in degraded mode. All data is still accessible (btrfs RAID1 keeps a copy on the surviving disk(s)), but the pool is running with reduced redundancy until you replace the dead drive.

### Hot-unplug while pool is mounted

If a drive is physically disconnected while the pool is mounted, its LUKS
mapper can remain open with `cryptsetup status` reporting `device: (null)`.
btrfs continues to list the devid but has not yet promoted it to MISSING.
`braid status` reports the devid -- it contributes to `missing_count` and
appears in `missing_devids` -- but `braid remove-missing --missing-id N` and
`braid replace` (with or without `--missing-id`) refuse the devid because they
only act on btrfs-authoritative MISSING entries.

To make progress:

1. Confirm the disk is truly gone (not just a loose cable).
2. Relock and re-unlock the pool degraded so btrfs re-evaluates membership and
   promotes the devid:
   ```sh
   sudo braid lock
   sudo braid unlock --allow-degraded
   ```
3. Re-run `braid status` -- the devid should now appear as authoritatively
   MISSING -- then retry `braid remove-missing` or `braid replace`.

### Option A: Replace the disk

Replaces the dead disk with a new one, rebuilding data from surviving copies:

```sh
sudo braid replace --old toshiba2 \
  --new toshiba4=/dev/disk/by-id/ata-NEW_DRIVE_SERIAL
```

`--old` identifies the missing member. If you want to cross-check the btrfs
devid from `braid status`, add `--missing-id` after the required args:

```sh
sudo braid replace --old toshiba2 \
  --new toshiba4=/dev/disk/by-id/ata-NEW_DRIVE_SERIAL \
  --missing-id 3
```

Replace runs `btrfs replace start -B` under the hood. `braid replace` is a long-running online operation: the command waits in the foreground and shows progress while the pool remains usable. It can take hours for large drives, so run it from a shell you can leave open (or a `tmux`/`screen` session). From another shell, `braid status` and `braid tui` can show progress independently.

### Option B: Remove the missing device

Forgets the dead device without rebuilding data:

```sh
# Find the missing device's btrfs devid from braid status
sudo braid remove-missing --missing-id 3
```

Use this when you do not have a replacement disk. The pool continues with fewer disks and reduced capacity. Data that was only on the dead drive is lost (but in RAID1, all data has a second copy on another drive).

When this clears the last missing device and 2+ disks remain, `remove-missing` blocks on a follow-up soft RAID1 balance to restore redundancy on chunks written as `single` during degraded operation. You will see `[wait] pool: restoring RAID1 redundancy...` then `[ok]   pool: RAID1 redundancy restored` before the command returns. The wait scales with how much data was written while degraded: an idle pool finishes in seconds, while a pool written to heavily during degraded mode can take longer. A sleep inhibitor is held for the entire operation. See [`braid remove-missing`](../commands/remove-missing.md) for the full sequence.

Verify:

```sh
sudo braid status
```

A successful result shows no missing devices and no `single` profile rows for data or metadata.

### Choosing between replace and remove-missing

| | `replace` | `remove-missing` |
| --- | --- | --- |
| Requires new disk | Yes | No |
| Rebuilds data | Yes | No |
| Restores redundancy | Yes | Partial: restores RAID1 profiles when 2+ disks remain, but does not add replacement capacity |
| Duration | Hours (large disks) | Minutes |
| When to use | You have a replacement | No replacement available |

## Degraded mount

A degraded mount means at least one pool disk is missing. The pool is usable but the pool is running with reduced redundancy on the missing device's share of data.

### When degraded mounts happen

- `braid unlock --allow-degraded` -- explicit request
- `braid recover --allow-degraded` -- recovery with missing devices
- `braid.autoUnlock.allowDegraded = true` -- auto-unlock config

### Risks

- **Reduced redundancy** -- the pool is short the missing device's mirror copy of existing data, and on 2-disk pools new writes are allocated as single-profile chunks. A further drive failure could lose data.
- **No self-healing** -- btrfs cannot repair corrupted blocks from a redundant copy if the copy was on the missing device.

### Resolution

Replace the missing disk as soon as possible:

```sh
sudo braid replace --old <missing-name> \
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
├── "LUKS UUID mismatch" error
│   └── see "Unlock refused by a foreign or mismatched disk"
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
| `pool.json` | UUID-keyed pool membership; each value stores disk name, by-id path, prior devid, and added-at timestamp |
| `pending-op.json` | UUID-keyed pending operation journal (present only during mutations) |
| `acked-stats.json` | Acknowledged btrfs device stats baseline |
| `smartd-alert` | Flag file set by smartd alert script |
| `scrub-deferred` | Flag file marking a scheduled scrub that was skipped because braid was busy with the pool; cleared when a scrub actually runs |
| `alert-latch.json` | Active alert state |
| `luks-headers/` | LUKS header backups |

## Related

- [Troubleshooting](troubleshooting.md) -- symptom-oriented quick fixes
- [NixOS configuration](nixos-configuration.md) -- `autoUnlock.allowDegraded` and other options
