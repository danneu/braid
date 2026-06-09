[← braid](../index.md)

# Troubleshooting

Symptom-oriented index for common problems. Find your symptom below and follow the resolution.

## Balance fails with "No space left on device"

btrfs balance needs temporary free space to relocate chunks. Braid balances
convert both data and metadata profiles, so either side can hit ENOSPC even
when there appears to be space available.

**Fix:** Free up empty data block groups first, then retry the original operation:

```sh
sudo btrfs balance start -dusage=0 /mnt/storage
```

The `usage=0` pass relocates only completely empty data block groups, so it does
not need temporary work space. Keep recovery balances data-only: metadata block
groups are write headroom, and balancing them can hit metadata ENOSPC and force
the filesystem read-only.

If the retry still fails, inspect data vs metadata usage:

```sh
sudo btrfs filesystem usage /mnt/storage
```

`df`'s "Used" and "Available" columns cannot distinguish data, metadata, and
snapshot references, while `braid status` reports the same btrfs-derived
capacity. In `btrfs filesystem usage`, compare the Data and Metadata used/size
ratios to see which side is the bottleneck.

If there is enough temporary work space, a non-zero data threshold can reclaim
nearly-empty groups, but it moves data:

```sh
sudo btrfs balance start -dusage=10 /mnt/storage
```

## Pool won't mount

**Symptom:** `braid unlock` fails because pool.json is missing or corrupted.

**Fix:** Rebuild UUID-keyed pool.json from disk labels and LUKS UUIDs. How you
start depends on the state of `pool.json` -- bare `discover` previews only when
the file is absent; over a corrupt file it refuses and points you to
`discover --write`.

**If `pool.json` is missing** -- preview, then write:

```sh
sudo braid discover
# Shows discovered disks -- verify they look correct
sudo braid discover --write
```

**If `pool.json` is corrupt or unreadable** -- skip the preview and rebuild in
place (bare `discover` refuses corrupt state before scanning):

```sh
sudo braid discover --write
```

The corrupt rebuild preserves the original bytes at
`pool.json.corrupt-<RFC3339-UTC>` before overwriting; do not remove it first.

Then unlock normally:

```sh
sudo braid unlock
```

`discover` scans `/dev/disk/by-id/` for LUKS devices with `braid-*` labels and reconstructs the membership file. See [Recovery scenarios](recovery-scenarios.md) for details.

If `pool.json` is healthy and UUID-keyed, `discover --write` refuses on
purpose. Use `braid add` / `braid remove` / `braid replace` for normal
membership changes. If you have deliberately decided to re-discover instead,
move the file aside before running `discover --write`:

```sh
sudo mv /var/lib/braid/pool.json /var/lib/braid/pool.json.manual-backup
sudo braid discover --write
```

## Interrupted operation (pending-op.json exists)

**Symptom:** braid commands fail with an error about a pending operation. This happens when a previous `add`, `remove`, `remove-missing`, or `replace` was interrupted (power loss, crash, killed process).

**Fix:** Use `braid recover`:

```sh
sudo braid recover
```

Recover reads the pending-operation journal, opens LUKS devices and mounts the pool if needed, probes the live btrfs topology, and rebuilds pool.json from actual state. It clears the journal only after the idle/no-paused recovery path succeeds.

> [!IMPORTANT]
> If recover refuses owed RAID1 replay because btrfs balance state is paused, running, or unknown, it left `pending-op.json` in place. Inspect btrfs manually before clearing recovery state.

If devices are missing (drive failure during the interrupted operation):

```sh
sudo braid recover --allow-degraded
```

For scripted/unattended recovery:

```sh
echo "my-passphrase" | sudo braid recover --passphrase-stdin
```

See [Recovery scenarios](recovery-scenarios.md) for detailed walkthroughs.

## Missing device after drive failure

**Symptom:** `braid status` shows a missing device. The pool may be mounted degraded or may fail to mount.

You have two options:

### Option A: Replace the disk (rebuilds data onto a new disk)

```sh
# Find the old disk name from braid status
sudo braid replace --old toshiba2 \
  --new toshiba4=/dev/disk/by-id/ata-NEW_DRIVE_SERIAL
```

Replace copies data from surviving redundant copies onto the new disk. This restores full RAID1 redundancy. It takes hours for large disks.

### Option B: Forget the missing device (no data rebuild)

```sh
# Find the missing device's btrfs devid from braid status
sudo braid remove-missing --missing-id 3
```

This removes the dead device entry from the btrfs filesystem. No data is rebuilt -- you lose the redundant copy that was on the dead drive. The pool continues as a smaller array. Use this when you do not have a replacement disk available.

## Auto-unlock fails

**Symptom:** Pool is not unlocked after reboot despite auto-unlock being configured.

Check the service logs:

```sh
journalctl -u braid-auto-unlock.service
```

Common causes:

- **USB device not found:** The USB drive was not plugged in or the `keyDevice` path is wrong. Verify with `ls /dev/disk/by-id/ | grep usb`.
- **Keyfile not found:** The USB filesystem does not contain `braid.key` at the root. The file must be named exactly `braid.key`.
- **Keyfile resolves outside mount:** A symlink on the USB points outside `/run/braid-key/`. The service refuses this for security.
- **Timeout too short:** The USB device takes longer to enumerate than `timeoutSec`. Increase it in your NixOS config.
- **Missing devices:** If a pool disk is dead and `allowDegraded = false` (the default), auto-unlock exits with code 2. Set `braid.autoUnlock.allowDegraded = true` to allow degraded mount.

See [Auto-unlock](auto-unlock.md) for the setup guide.

## Beeper won't stop

**Symptom:** The PC speaker is beeping (initially every few seconds, then less often) due to a disk health alert.

**Fix:** Acknowledge the alert:

```sh
sudo braid ack
```

This stops the beep loop and clears the alert state. Then investigate the underlying problem:

```sh
sudo braid status
sudo braid doctor
```

## braid commands blocked by "another operation in progress"

**Symptom:** `braid unlock`, `braid add`, or `braid recover` fails with a message about another braid operation holding the pool lock.

The pool-mutating commands acquire an exclusive lock on `/run/braid-pool.lock`. If a previous command is still running (or crashed without releasing the lock), new commands fail fast.

**Fix:** Wait for the running command to finish. If the previous command crashed, the lock file is released automatically (it is a `flock` on a `/run/` file, which is tmpfs and cleared on reboot). If you need to proceed before a reboot:

```sh
# Check if any braid process is still running
ps aux | grep braid
# If nothing is running, the lock was released — retry your command
```

## Scrub won't start

**Symptom:** `systemctl status braid-scrub.timer` shows the timer is inactive.

The scrub timer is lifecycle-bound to `braid-online.service`. It only runs while the pool is unlocked and mounted.
If a scrub was cancelled by lock or shutdown, braid resumes the partial scrub
the next time the pool comes online.

```sh
# Check pool state
sudo braid status
# If pool is offline, unlock it
sudo braid unlock
# Timer should now be active
systemctl status braid-scrub.timer
```

## Scrub reported errors

**Symptom:** `braid status` shows `Last scrub: <ts> (N errors)` or
`braid monitor` raised a btrfs error alert after a scrub.

The scrub error count braid reports is authoritative -- braid parses it from
`btrfs scrub status`. Journal lines are diagnostic *clues*, not a complete
per-error ledger: the kernel emits scrub messages through rate-limited helpers,
so a busy or bursty scrub can produce fewer journal lines than the count. A
non-zero count with sparse or missing journal lines is not a braid bug -- it
usually means the kernel dropped log entries to stay under its rate limit.

Use the command printed under the scrub status, or run journalctl directly:

```sh
sudo journalctl -k --since '<scrub-start-time>' --grep 'BTRFS.*(at logical.*on (dev|mirror)|super block at physical)'
```

Output comes in two distinct grammars depending on whether the error is in a
data/metadata extent or in a superblock copy.

**Extent errors (data and metadata).** Each affected sector may log a
repair-summary line:

- Corrected via RAID1 mirror: `fixed up error at logical N on dev
  /dev/mapper/braid-X physical N` (or `... on mirror N` when the source mirror
  has no device). btrfs RAID1 read the healthy mirror and wrote it back over
  the bad copy. **No file path** -- corrected lines give block coordinates
  only. A count consisting mostly of `fixed up error` lines means data
  integrity was preserved; investigate the disk that produced the bad reads.
- Uncorrectable: `unable to fixup (regular) error at logical N on dev X
  physical N` (or `... on mirror N`). RAID1 could not recover -- the mirror was
  also bad or no mirror exists. The block is permanently damaged.

An uncorrectable extent error *may* also log an additional detail line that
identifies what was lost. The detail emission is gated by a second rate-limit
check, so it is not guaranteed to appear for every uncorrectable error. When
present, the shapes are:

- **Data extent, path resolved.** `... at logical N on dev X, physical N, root
  N, inode N, offset N, length N, links N (path: subdir/victim.bin)`. `(path:
  ...)` is **relative to the affected btrfs subvolume root**, not absolute. The
  kernel builds it from `paths_from_inode()`
  (`reference/linux/fs/btrfs/scrub.c:457`,
  `reference/linux/fs/btrfs/backref.c:2125`) and does not know what mount point
  exposes that subvolume. Prepend the mount point of the affected subvolume
  (default subvolume at `/mnt/storage`; named subvolumes wherever you
  configured them).
- **Data extent, path resolution failed.** Same shape but ends `... path
  resolving failed with ret=N` instead of `(path: ...)`. Usually means the
  extent has no remaining inode references (file already deleted) or the inode
  lives in a snapshot rooted under a different subvolume than the search root.
- **Metadata.** `... at logical N on dev X, physical N: metadata leaf|node
  (level N) in tree N`. Tree-block corruption -- no file path because the bad
  block lives in a btrfs tree, not in user data. Persistent metadata errors
  indicate disk failure.

**Superblock errors.** Logged as standalone messages from `scrub_supers`, not
as repair-summary + detail pairs. The grammar is independent of the extent
path:

- `super block at physical N devid N has bad csum`
- `super block at physical N devid N has bad generation N expect N`

Damage to one of the device's superblock copies. Investigate the device
(identified by `devid`), not a file.

For the **path-resolution-failed** case, you can try `inode-resolve` as a
best-effort:

```sh
sudo btrfs inspect-internal inode-resolve <inode> /mnt/storage
```

This succeeds only if the inode still exists in the subvolume rooted at the
supplied path. Deleted files, extents with no remaining references, or files
that live in a different subvolume will still produce no result -- the kernel
logged "path resolving failed" for the same reason.

A non-zero error count after a scrub means at least one block failed its
checksum or I/O. With btrfs RAID1, blocks with a healthy mirror are repaired
automatically (counted as `Corrected` -- the `fixed up` lines above);
`Uncorrectable` means both copies were bad and the file (for data) or tree
block (for metadata) is now damaged. The journal output is your best diagnostic
surface, but treat it as evidence rather than a complete ledger: rely on the
scrub count for "how many," and on the journal for "what kind, and where the
kernel could log it." Restore affected files from backup and run `braid ack`
once you have investigated.

## SMB/NFS service inactive after `braid lock`

**Symptom:** `systemctl status samba-smbd.service` (or `nfs-server.service`) shows `inactive (dead)` immediately after you ran `braid lock`.

This is intentional. On NixOS module installs, `braid lock` stops every service bound to `braid-online.service` via `BindsTo=braid-online.service` before it unmounts the pool. The cascade prevents busy-mount unmount failures.

**Fix:** Run `braid unlock`. It reactivates `braid-online.service` after mount, and systemd restarts every consumer that is also `WantedBy=braid-online.service`.

If the service does not restart on `braid unlock`, it is wired for the stop side (`BindsTo`) but not the start side (`WantedBy`). The recommended setup wires the share into the full pool lifecycle -- see [Binding shares to the pool lifecycle](sharing-and-permissions.md#binding-shares-to-the-pool-lifecycle).

## Related

- [Recovery scenarios](recovery-scenarios.md) -- detailed recovery walkthroughs
- [NixOS configuration](nixos-configuration.md) -- module option reference
- [Monitoring and alerts](monitoring-and-alerts.md) -- alert system details; see "Scrub reported errors" above for the post-alert investigation steps.
