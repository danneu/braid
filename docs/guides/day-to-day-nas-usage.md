[← braid](../index.md)

# Day-to-day NAS usage

This guide covers normal operation: the reboot cycle, checking on your pool, adding disks over time, organizing your data with subvolumes, and good operator habits.

Read this after you have completed [Getting started](getting-started.md) and have a working pool.

## The daily cycle

A typical NAS session looks like this:

```
NAS powers on
  -> boots to login (pool offline, drives encrypted)
  -> SSH in
  -> sudo braid unlock (enter passphrase)
  -> pool online at /mnt/storage
  -> use it (copy files, stream media, backups)
  -> pool locks automatically on shutdown
```

If you have [auto-unlock](auto-unlock.md) configured with a USB keyfile, the unlock step happens automatically at boot.

### Unlock

```sh
ssh user@nas
sudo braid unlock
```

braid prompts for your LUKS passphrase, opens all drives, and mounts the pool.

### Use the pool

The pool is a normal directory at `/mnt/storage` (or whatever you set `braid.mountPoint` to). Copy files, create directories, share via Samba/NFS -- it works like any other filesystem.

```sh
cp ~/photos/* /mnt/storage/photos/
rsync -av ~/documents/ /mnt/storage/documents/
```

### Lock

```sh
sudo braid lock
```

This unmounts the pool and closes all LUKS devices. The pool locks automatically on shutdown, so manual locking is only needed if you want drives encrypted while the machine stays on.

## Checking pool health

Run `braid status` periodically to check on your pool:

```sh
sudo braid status
```

This shows pool state, disk health, allocation, scrub history, and any active alerts. Make a habit of glancing at this after unlocking, especially if the NAS has been running unattended.

For an interactive view:

```sh
sudo braid tui
```

The TUI dashboard shows pool health, disk status, balance progress, and SMART data in a live-updating terminal interface.

## Organizing data with subvolumes

btrfs subvolumes are the right way to organize different categories of data on your NAS. Think of them as lightweight partitions within the pool.

### Subvolumes vs directories

A plain directory works fine for storing files, but subvolumes give you:

- **Independent snapshots** -- snapshot your documents without snapshotting your movie library.
- **Per-subvolume quotas** -- limit how much space a category can use (optional).
- **Selective backup** -- send/receive individual subvolumes to an external drive.
- **No cost upfront** -- subvolumes are free to create. They share the pool's space with no pre-allocated size.

There is no downside to creating subvolumes early. If you later decide you do not need snapshots for a category, the subvolume still works exactly like a directory.

### Creating subvolumes

Create subvolumes for your major data categories:

```sh
sudo btrfs subvolume create /mnt/storage/documents
sudo btrfs subvolume create /mnt/storage/photos
sudo btrfs subvolume create /mnt/storage/movies
sudo btrfs subvolume create /mnt/storage/music
sudo btrfs subvolume create /mnt/storage/backups
```

Then use them like normal directories:

```sh
cp ~/report.pdf /mnt/storage/documents/
rsync -av ~/Photos/ /mnt/storage/photos/
```

### Snapshots

Snapshot a subvolume to create a point-in-time copy:

```sh
sudo btrfs subvolume snapshot -r /mnt/storage/documents /mnt/storage/.snapshots/documents-2026-04-09
```

The `-r` flag makes it read-only, which is best practice for backup snapshots. Snapshots are nearly instant and use no extra space until the original data changes. Deleting a file from the original subvolume does not reclaim its blocks while any snapshot still references them. To free that space, delete the snapshots holding the data with `sudo btrfs subvolume delete /mnt/storage/.snapshots/<name>`.

### Listing subvolumes

```sh
sudo btrfs subvolume list /mnt/storage
```

To mount a specific subvolume at a custom path, for example for a service or a
friendlier path under `/home`, see [Mounting subvolumes](mounting-subvolumes.md).

## Adding disks over time

You can add new drives to an existing pool without rebuilding or reformatting:

```sh
# Find the new drive
lsblk -d -o NAME,SIZE,MODEL,ID-LINK

# Add it
sudo braid add newdisk=/dev/disk/by-id/ata-NEWDISK_SERIAL
```

braid formats the new drive with LUKS (using your existing passphrase), adds it to the btrfs pool, and rebalances data across all drives. No `nixos-rebuild` required.

The balance runs in the foreground -- `braid add` holds the terminal and does not return until it finishes, which can take hours on a large pool. braid shows live balance progress while it runs.

btrfs RAID1 keeps exactly 2 copies of every block no matter how many drives the pool has. A 3rd or 4th drive gives you more usable capacity, but it does not increase fault tolerance -- the pool still tolerates a single drive failure, the same as a 2-drive pool. See [Decision 001](../design/decisions/001-btrfs-raid1.md) for the rationale.

## Responding to alerts

If the NAS beeps (or sends you an alert via a custom command), something needs attention:

1. SSH in and check status:

   ```sh
   sudo braid status
   ```

2. The status output shows what triggered the alert: btrfs device errors, a missing disk, or a SMART warning.

3. Investigate and fix the issue (replace a failing disk, check cables, etc.).

4. Once resolved, acknowledge the alert to silence it:

   ```sh
   sudo braid ack
   ```

See [Monitoring and alerts](monitoring-and-alerts.md) for details on how alerts work.

## Good operator habits

- **Check `braid status` after unlocking** -- a quick glance catches problems early.
- **Keep LUKS header backups** -- braid stores header backups in `/var/lib/braid/luks-headers/` after operations that modify LUKS headers. Copy each `.luksheader` file off the NAS to a separate location, then delete the local file (`braid status` warns until they are removed). If a drive's LUKS header is corrupted and you have no off-system backup, the data on that drive is unrecoverable.
- **Run `braid doctor`** -- periodically check for configuration problems:
  ```sh
  sudo braid doctor
  ```
- **Let scrubs complete** -- braid runs monthly scrubs by default. Scrubs verify every block's checksum and repair corruption from redundant copies. braid starts them at low CPU priority (`Nice=19`) and idle I/O priority (`IOSchedulingClass=idle`). The CPU priority always applies; the I/O priority is best-effort -- how strongly the kernel honors it depends on your block-layer I/O scheduler -- so do not treat it as a guarantee that scrubs will never affect interactive workloads. The pool stays online throughout. If scrubs noticeably impact Samba, NFS, or local use on your hardware, retime them with `braid.autoScrub.interval` (any systemd calendar expression -- e.g. `"Sun *-*-* 02:00:00"`) to land in an off-peak window. Do not interrupt a scrub in progress.
- **Create subvolumes early** -- there is no cost to creating them upfront, and you cannot convert a directory to a subvolume later without copying the data.

## What's next

- [Auto-unlock](auto-unlock.md) -- skip the manual passphrase step on boot
- [Mounting subvolumes](mounting-subvolumes.md) -- expose a subvolume at a custom path
- [Monitoring and alerts](monitoring-and-alerts.md) -- automatic health checking and notifications
- [Power management](power-management.md) -- suspend when idle, wake on demand

## Related commands

- [unlock](../commands/unlock.md)
- [lock](../commands/lock.md)
- [status](../commands/status.md)
- [add](../commands/add.md)
- [tui](../commands/tui.md)
- [doctor](../commands/doctor.md)
- [ack](../commands/ack.md)
