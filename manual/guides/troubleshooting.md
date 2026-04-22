[← Manual](../index.md)

# Troubleshooting

Symptom-oriented index for common problems. Find your symptom below and follow the resolution.

## Balance fails with "No space left on device"

btrfs balance needs temporary free space to relocate data. If the pool is very full, balance fails with ENOSPC even when there appears to be space available.

**Fix:** Free up empty block groups first, then retry:

```sh
sudo btrfs balance start -dusage=0 /mnt/storage
sudo btrfs balance start -dusage=10 /mnt/storage
# Then retry the original operation (e.g. braid remove)
```

The `-dusage=0` pass relocates only completely empty block groups (zero cost). `-dusage=10` relocates nearly-empty groups. This frees enough contiguous space for the full balance to proceed.

## Pool won't mount

**Symptom:** `braid unlock` fails because pool.json is missing or corrupted.

**Fix:** Rebuild pool.json from disk labels:

```sh
sudo braid discover
# Shows discovered disks — verify they look correct
sudo braid discover --write
# Then unlock normally
sudo braid unlock
```

`discover` scans `/dev/disk/by-id/` for LUKS devices with `braid-*` labels and reconstructs the membership file. See [Recovery scenarios](recovery-scenarios.md) for details.

**Note:** `discover` refuses to run if pool.json already exists. If pool.json exists but is wrong, remove it first:

```sh
sudo rm /var/lib/braid/pool.json
sudo braid discover --write
```

## Interrupted operation (pending-op.json exists)

**Symptom:** braid commands fail with an error about a pending operation. This happens when a previous `add`, `remove`, `remove-missing`, or `replace` was interrupted (power loss, crash, killed process).

**Fix:** Use `braid recover`:

```sh
sudo braid recover
```

Recover reads the pending-operation journal, opens LUKS devices and mounts the pool if needed, probes the live btrfs topology, and rebuilds pool.json from actual state. It then clears the journal.

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

```sh
# Check pool state
sudo braid status
# If pool is offline, unlock it
sudo braid unlock
# Timer should now be active
systemctl status braid-scrub.timer
```

## Related

- [Recovery scenarios](recovery-scenarios.md) -- detailed recovery walkthroughs
- [NixOS configuration](nixos-configuration.md) -- module option reference
- [Monitoring and alerts](monitoring-and-alerts.md) -- alert system details
