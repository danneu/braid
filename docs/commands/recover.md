---
experimental: true
---
[← braid](../index.md)

# braid recover

{{#include ../_includes/experimental-command-callout.md.inc}}

Resumes from an interrupted operation (add, remove, replace) by opening LUKS devices, mounting the pool, rebuilding `pool.json` from live pool state when appropriate, running owed maintenance when the btrfs balance state is idle (the owed RAID1 balance also needs a pool that can host RAID1), and clearing the pending-operation journal only after the safe recovery path completes.

## When to use it

- After a crash, power failure, or interrupted braid command.
- When `braid status` or other commands show "pending operation -- run `braid recover`".
- Only available when `pending-op.json` exists.

## Basic example

```
sudo braid recover
```

You'll be prompted for the pool passphrase. Output shows the recovery process:

```
Recovering from interrupted "add" operation (started 2026-03-15T14:30:00Z)...
pool.json written from completed add membership.
pool.json written from committed add membership.
pending-op.json cleared. Recovery complete.
```

Before the `pool.json` lines, a real run prints either per-disk LUKS-open and mount rows (if the pool was offline) or a single `pool already mounted at ...` row (if this pool was already mounted). On the idle/no-paused owed RAID1 path, recover probes the pool again. If every member is present and two or more remain, it prints a RAID1 balance replay row pair after the `committed` line and before the final `pending-op.json cleared` line. An add into an existing pool replays the hard convert live add runs (`replaying post-add RAID1 balance`). When the interrupted convert left mixed chunk profiles, the replay first prints `pool: finishing an interrupted RAID1 convert before the full rewrite...` and runs the soft convert, because a hard convert alone fails there with ENOSPC. `remove-missing`, `replace` and a bootstrap add of two or more disks replay the soft convert (`replaying post-<op> RAID1 soft balance`). If a member is missing or hot-unplugged, recover prints a `[skip]` line instead and still clears the journal: live add's degraded-balance skip line for the hard convert, or `[skip] pool: rebalance skipped -- redundancy not restored` for the soft convert, as the live `remove-missing` and `replace` restore does. The repair step (`replace` or `remove-missing`) restores redundancy later. A one-device pool skips the balance silently. If the balance check is paused, running, or unknown, recover fails before the replay row and does not clear the journal.

> [!IMPORTANT]
> If recover refuses owed RAID1 replay because btrfs balance state is paused, running, or unknown, it left `pending-op.json` in place. Inspect btrfs manually before clearing recovery state.

## Common variations

Non-interactive (passphrase from stdin):

```
echo -n 'my-passphrase' | sudo braid recover --passphrase-stdin
```

Passphrase from a file:

```
sudo braid recover --passphrase-file /root/passphrase.txt
```

Recover with a missing disk (degraded mode):

```
sudo braid recover --allow-degraded
```

Preview what would happen:

```
sudo braid recover --dry-run
```

## Flags

| Flag | Effect |
| --- | --- |
| `--passphrase-stdin` | Read passphrase from stdin instead of TTY prompt |
| `--passphrase-file <path>` | Read passphrase from a file instead of TTY prompt (conflicts with `--passphrase-stdin`) |
| `--allow-degraded` | Allow mounting with missing devices (redundancy is reduced until you replace the missing device) |
| `--dry-run` | Show what would be done without making changes |
| `--progress auto\|always\|never` | Control progress display (default: auto) |

## What happens under the hood

1. Loads `pending-op.json` (refuses if absent -- nothing to recover).
2. Chooses the mount membership from the journal phase. Existing-pool add and remove-missing `PoolMutation` phases mount from the pre-operation membership. Add, remove-missing, and replace post-maintenance phases mount from the committed target membership. Replace `PoolMutation`, bootstrap add `PoolMutation` (the first disk, whose pre-operation membership is empty), and `Remove` mount from the admission membership (pre-operation snapshot plus target-only members) -- for replace this matters because the kernel may still be completing `dev_replace`.

   Before that, a bootstrap add `PoolMutation` with nothing mounted at the mount point gets one more check. `braid add` runs mkfs.btrfs only after it formats every disk, so recover probes each journaled disk. A disk that is gone, or one with a LUKS header that lacks the `braid-<name>` label or the journaled LUKS UUID, refuses first, before recover decides whether mkfs.btrfs ran (see Safety checks). If no disk carries a LUKS header, nothing changed: recover clears `pending-op.json` without asking for a passphrase. If the formatted disks carry no btrfs, mkfs.btrfs never ran: recover opens each closed one to look, closes it again, clears `pending-op.json`, and lists the disks to wipe before you re-run `braid add`. If any of them carries btrfs, or recover cannot tell, it mounts as below. `--dry-run` does not open disks, so it previews the mount with a note.
3. Opens LUKS devices and mounts the pool (or reuses the existing mount if it is this pool: a btrfs whose device records the LUKS UUID of a disk in `pool.json` or the journal). **Exception:** a `Replace::PoolMutation` journal on an externally-mounted pool is refused (see Safety checks); replace post-maintenance recovery on an already-mounted pool is allowed.
4. For `Replace::PoolMutation` only, if a kernel-resumed btrfs replace is in progress, waits for it to finish.
5. For `Replace::PoolMutation` only, if the pool was just mounted by this recover run, performs a full relock-and-remount cycle (umount, `btrfs device scan --forget`, close LUKS, reopen, remount) to ensure the kernel's in-memory device topology matches the on-disk state.
6. Probes the live pool and binds it to the journaled membership in one pass, the same for every journal kind and phase. A device whose mapper is open binds by the LUKS UUID it records, with or without a backing disk. A device btrfs reports only as a MISSING devid binds to the member that records that devid. A MISSING devid no member records stays out of `pool.json`. Recover fails with the journal kept if such a devid sits beside a member that records no devid, because either could be the other.
7. For interrupted existing-pool add `PoolMutation`, first runs a non-destructive Add target reconciliation pass: any journaled add target whose underlying disk is physically present and LUKS-openable is opened, scanned, and followed by a live-pool re-probe. Targets that turn out to be live pool members are adopted into the recovered `pool.json` without `wipefs` or `btrfs device add`.
8. For add `PoolMutation`, acts only on journaled targets the live pool does not already hold. A target counts as held when an open mapper in the pool records its LUKS UUID, with or without a backing disk. The journal records no devid for a new disk until the advance to `PostAddBalanceRaid1`. So a committed new disk that is gone, which btrfs shows only as a MISSING devid, does not count as held: recover refuses at that disk and keeps `pending-op.json` until the disk returns. For each target not held, recover finishes the steps `braid add` planned for that kind of disk, fresh or returned; [Interrupted adds](add.md#interrupted-adds) lists them. Recover never formats a disk. A fresh target that is still not LUKS never reached btrfs, so recover drops it from `pending-op.json` before it replays any target; if no target remains, it writes the pre-operation `pool.json` and clears the journal. Re-run `braid add` for a dropped disk. A refusal keeps `pending-op.json` (see Safety checks).

   See [Pending LUKS header backups](status.md#pending-luks-header-backups) -- copy each `.luksheader` off-system and delete the local copy.
9. For add `PostAddBalanceRaid1`, does not format, enroll, back up headers as target prep, wipe, or add disks. It only checks that the live pool holds every member of the committed membership the journal records, at the devid btrfs gave each one, and runs the owed RAID1 balance, the same hard convert live add runs (after a soft pass if the pool holds mixed chunk profiles), when btrfs balance state is idle and the pool can host RAID1 (no member missing or hot-unplugged, two or more present); a paused, running, or unknown balance state fails closed with the journal preserved.
10. For replace and remove-missing `PoolMutation`, detects whether the primary btrfs membership mutation committed. It does not rerun `btrfs replace start` or `btrfs device remove`. If the live pool matches neither the pre-operation nor the committed membership, recover refuses and keeps `pending-op.json`.
    - **Replace that did not commit:** recover first finishes the prep `braid replace` planned for the new disk, except the format. It checks the passphrase against the pool members and the new disk, enrolls the keyfile if the journal records one, and backs up the LUKS header. [What happens under the hood](replace.md#what-happens-under-the-hood) says which new disks replace backs up. An existing-LUKS disk with no keyfile to enroll, and a fresh disk braid never formatted, get no write and no passphrase check. Then recover restores the pre-operation `pool.json`, clears the journal, and tells you to rerun `braid replace`. Before any of this, recover checks the new disk and can refuse with the journal kept (see Safety checks).
    - **Remove-missing that did not commit:** recover restores/keeps the pre-operation `pool.json`, clears the journal, and tells you to rerun the original command. On a 2-disk pool with one disk missing, the collapse's convert may be paused; recover leaves it alone and tells you to finish it with `btrfs balance resume <mount-point>` or stop it with `btrfs balance cancel <mount-point>` first, because `remove-missing` refuses while a balance is paused.
11. For replace and remove-missing post-maintenance phases, validates committed live membership, repairs `pool.json` if needed, and finishes only owed maintenance such as resize or, when btrfs balance state is idle and the pool can host RAID1, soft RAID1 balance; it does not rerun the primary btrfs membership mutation. A paused, running, or unknown balance state before owed RAID1 replay fails closed with `pending-op.json` preserved.
12. Resolves `/dev/disk/by-id/` paths from the live backing disk of each present member (not from the journal's by-id path, which may be stale). A hot-unplugged or MISSING member has no live backing disk, so it keeps its journaled path.
13. Writes or repairs `pool.json` only after the journal phase allows it and the live pool holds every member of the membership that phase expects. A member that is hot-unplugged or MISSING stays in `pool.json`, so `replace` or `remove-missing` can name it later. When an existing-pool add advances to `PostAddBalanceRaid1`, the journal records that membership as its target.
14. Clears `pending-op.json` only after membership is complete and any owed balance work is done or skipped because the pool cannot host RAID1.

## Safety checks

- Refuses if no `pending-op.json` exists.
- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
- Refuses to adopt live pool members outside the recovery admission membership for the current journal phase (guards against devices added outside braid). Most phases admit the pre-operation snapshot plus target-only members; `Replace::PostReplaceMaintenance` admits only the committed target membership because btrfs preserves the old device's devid on the replacement after commit.
- Hard-fails if a live pool device has no `/dev/disk/by-id/` symlink (recovery can't guess a stable identifier).
- For a bootstrap add with nothing mounted at the mount point, checks every journaled disk before it decides whether mkfs.btrfs ran. It refuses with the journal kept if a disk is absent (`recover add target '<name>' (<by-id path>) is not present; reconnect it`) or carries a LUKS header without the `braid-<name>` label or the journaled LUKS UUID. These refusals hold whether or not mkfs.btrfs ran, and `--allow-degraded` does not bypass them. If the add never reached mkfs.btrfs, recover clears `pending-op.json` instead of trying to mount a pool that does not exist. It refuses with the journal kept if it cannot close a mapper it opened to check for btrfs.
- Refuses to overwrite `pool.json` or clear `pending-op.json` if the post-mount probe at the configured mount point sees the pool mounted read-only -- at the VFS layer or in the filesystem's own mount options. `pool.json` and `pending-op.json` are both preserved. If an operator or a tool remounted the pool read-only, `mount -o remount,rw <mount-point>` fixes that; then re-run `braid recover`. If btrfs forced it read-only after an error, that remount fails and only a fresh mount clears the error: fix the cause, then run `braid lock` and `braid recover`. See [Pool is mounted read-only](../guides/troubleshooting.md#pool-is-mounted-read-only).
- Refuses to overwrite `pool.json` or clear `pending-op.json` if the post-mount probe at the configured mount point sees the pool unmounted or with zero btrfs devices. The mount may have been removed externally between recover's mount step and membership probe; `pool.json` and `pending-op.json` are both preserved -- investigate the mount, then re-run `braid recover`.
- For existing-pool add recovery, refuses to clear the journal while the live pool does not hold every journaled add target, or does not hold a member the add started with.
- For existing-pool add and replace recovery, checks each journaled target disk before it enrolls, backs up, adds, drops, or abandons that disk. The disk must be present and must be the disk the journal recorded. A returned add disk, or an existing-LUKS replace disk, must still be LUKS with the journaled LUKS UUID. A returned disk that still shows a btrfs signature must show this pool's FSID. A fresh disk must either not be LUKS yet or carry the `braid-<name>` label and the journaled LUKS UUID. If a disk fails a check, recover refuses at that disk and keeps `pending-op.json`. A wrong passphrase refuses too. Reattach the right disk, then rerun `braid recover`. The label is only a sanity check; the LUKS UUID is the disk's identity ([ADR 024](../design/decisions/024-luks-uuid-identity.md#runtime-handles-and-labels)).
- For existing-pool add recovery, refuses to drop a fresh disk that is not LUKS while btrfs reports a MISSING devid that no pre-operation member records, or counts more devices than braid can see. Such a disk could be one btrfs already holds whose LUKS header now reads badly. Recover keeps `pending-op.json`; reconnect the missing disk and rerun `braid recover`.
- Returned-disk replay may need a pool passphrase even when the pool is already mounted, because the mapper for the journaled target may still be closed.
- Refuses if something other than this pool is mounted at the mount point, or if braid cannot tell.
  - A filesystem that is not btrfs, or a btrfs that is provably not this pool: its device is not a LUKS mapper, or its LUKS UUID is in neither `pool.json` nor the journal. Unmount it yourself (`sudo umount /mnt/storage`); `braid lock` refuses it too.
  - A mount braid cannot identify, for example because `pool.json` is corrupt, the device-mapper entry is unreadable, or the mount source is a `/dev/disk/by-id/...` link or btrfs's `<missing disk>`. Run `sudo braid lock`, which treats the mount as the pool and unmounts it.

  Then re-run `braid recover`. With the mount point empty, recover mounts the pool from the journal's membership.
- Without `--allow-degraded`, refuses to mount if devices are missing, and exits 2. A bootstrap add with nothing mounted refuses a missing disk earlier, at the bootstrap check above, and exits 1. Every other recover error exits 1. Argument errors, such as `--passphrase-file` with `--passphrase-stdin`, also exit 2, but clap prints its own diagnostic for them and never the `refusing to mount degraded` text a degraded refusal prints. Read stderr to tell the two exit-2 cases apart.
- Refuses to recover `Replace::PoolMutation` when the pool is already mounted (admin-mounted, circumventing braid's pending-op preflight on `unlock`). The kernel may have resumed an interrupted `dev_replace` on that mount session, leaving stale in-memory device state that recover cannot scrub without unmounting -- which it will not do on a mount it does not own. Remediation: `sudo braid lock; sudo braid recover`.

## Related commands

- [status](status.md) -- shows pending operation state and prompts you to recover
- [discover](discover.md) -- rebuild UUID-keyed pool.json from LUKS labels and UUIDs (when there's no journal)
- [unlock](unlock.md) -- normal unlock (when no journal exists)

## Related guides

- [Recovery scenarios](../guides/recovery-scenarios.md)
