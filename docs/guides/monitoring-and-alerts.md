[← braid](../index.md)

# Monitoring and alerts

This guide covers how braid monitors disk health and notifies you when something goes wrong.

Read this if you want to understand the alert system, configure notifications, or respond to an alert.

## How monitoring works

braid runs a health check every 5 minutes via a systemd timer. The check looks at four things:

1. **btrfs device stats** -- non-zero error counters (read, write, flush, corruption, generation errors) on any drive.
2. **Missing devices** -- a drive that should be in the pool but is not present.
3. **SMART alerts** -- smartd detected a SMART health warning on a drive.
4. **Scrub failures** -- the scheduled maintenance scrub failed to run or complete. `braid-scrub.service` raises this via `onFailure` (it writes a flag and starts the beeper); the monitor latches it on the next cycle.

A scrub that discovers unrepairable read, checksum, or generation errors increments the same btrfs device stats, so it follows the same beep and `braid status` flow as an everyday I/O error. That is distinct from check 4: a *failed* scrub (one that could not run or complete) is what raises the scrub-failure alert, while corruption a scrub *found* surfaces through check 1.

Separately, the monitor runs a proactive **capacity (ENOSPC) risk** check: it warns when an intact pool is one disk-loss away from being unable to allocate the RAID1 chunk pairs needed to restore redundancy. Unlike the four checks above, this is a *non-beeping* advisory (monitor exit 3) -- it runs your alert command and shows in `braid status`, but never beeps. Acknowledging it *snoozes* the reminder for 7 days rather than resolving it: if the pool is still at risk when the interval elapses the monitor reminds again, `braid status` keeps showing the advisory the whole time, and it re-arms automatically once you add capacity or free space.

If any check triggers, braid activates an alert.

### What happens on alert

When `braid monitor` detects a Critical issue (exit code 1), the systemd wrapper starts `braid-alert.service`, which:

- **Starts `braid-beep.service`** (if enabled). That service beeps the PC speaker until acknowledged. The cadence starts at 5 seconds and backs off exponentially (5s, 10s, 20s, 40s, ...) up to once every 15 minutes, so the early beeps are urgent but an ignored alert doesn't stay obnoxious.
- **Runs your custom alert command** (if configured), bounded by a timeout.

The beeping is intentionally persistent and annoying -- you should not be able to ignore a disk problem on a NAS that holds your data.

### Alerts are latched

An alert stays active until you acknowledge it with `braid ack`, even if the triggering condition goes away. This is by design: "a disk had errors" is worth investigating even if the error count stopped growing.

## Configuration

Monitoring is on by default when `braid.enable = true`. Here is the full set of options:

```nix
braid = {
  enable = true;

  monitor = {
    enable = true;        # default: true
    interval = "5min";    # default: "5min" (systemd time span)
    beep = true;          # default: true (PC speaker alert)
    alertCommand = null;  # default: null (optional custom command)
    alertCommandTimeoutSec = 60; # default: 60
  };
};
```

### Options

| Option | Default | Description |
| --- | --- | --- |
| `monitor.enable` | `true` | Enable disk health monitoring |
| `monitor.interval` | `"5min"` | How often to check (systemd time span: `"5min"`, `"30s"`, `"1h"`) |
| `monitor.beep` | `true` | Beep the PC speaker on alert |
| `monitor.alertCommand` | `null` | Custom command to run on alert (in addition to beep) |
| `monitor.alertCommandTimeoutSec` | `60` | Seconds before braid stops a custom alert command |

### Custom alert commands

Set `monitor.alertCommand` to run a script when an alert fires. This runs in addition to (not instead of) the beep:

```nix
braid.monitor.alertCommand = "/home/user/scripts/send-pushover-alert.sh";
```

The command runs as root. It is bounded by `monitor.alertCommandTimeoutSec` (default 60 seconds) on both the Critical path (`braid-alert.service`) and the Warning path (`braid-alert-advisory.service`), so a hung notifier cannot stall the alert latch or wedge the timer-driven monitor service. It should be idempotent; it runs once per alert episode until you acknowledge the alert.

### Disabling the beep

If you do not have a PC speaker or prefer silent alerts:

```nix
braid.monitor.beep = false;
```

You probably want to set a custom `alertCommand` if you disable the beep, otherwise alerts are silent and only visible in `braid status`.

## SMART integration

braid automatically configures smartd to monitor all drives. When smartd detects a SMART health issue, it writes a flag file that braid's monitor picks up on the next cycle.

You do not need to configure smartd yourself -- braid sets it up with sensible defaults. The NixOS `services.smartd` options are still available if you need to customize behavior.

## Alert workflow

When the NAS beeps (or your alert command fires):

### 1. SSH in and check status

```sh
ssh user@nas
sudo braid status
```

The output shows a banner when alerts are active and lists the causes:

- **BtrfsDeviceErrors** -- a specific drive has non-zero error counters. Could be a bad cable, a dying drive, or a transient issue.
- **MissingDevice** -- a drive is missing from the pool. Check if a cable came loose or if the drive failed.
- **SmartdAlert** -- SMART reports a health warning. The drive may be failing.
- **ScrubFailed** -- the scheduled scrub failed to run or complete. Check `journalctl -u braid-scrub.service` for the cause (a btrfs internal error, a device error that aborted the scrub, or metadata ENOSPC). Note that most entries in that journal are *not* failures: the scrub timer polls hourly, and a poll that finds the pool already fresh, finds a scrub already running, or skips because braid is busy with the pool all exit cleanly and raise nothing. See [Troubleshooting](troubleshooting.md#my-scrub-didnt-run) for how each reads.

### 2. Investigate

For device errors, check if they are growing:

```sh
# Wait a few minutes and check again
sudo braid status
```

Steady error counts after a reboot are often transient (power event, cable issue). Growing counts mean the drive is failing.

For a missing device, check physical connections. If the drive is dead, plan a replacement:

```sh
sudo braid replace --old deadname --new newname=/dev/disk/by-id/ata-NEW_SERIAL
```

### 3. Acknowledge

Once you have investigated and resolved (or accepted) the issue:

```sh
sudo braid ack
```

This silences the beep and resets the device-error baseline; new errors after ack trigger a fresh alert. A capacity (ENOSPC) warning is instead *snoozed* for a reminder interval -- ack does not resolve it, and `braid status` keeps showing it until you add capacity or free space.

## Checking monitor status

View the monitor service logs:

```sh
journalctl -u braid-monitor.service --since "1 hour ago"
```

View the alert orchestrator:

```sh
journalctl -u braid-alert.service
```

View the beep loop:

```sh
journalctl -u braid-beep.service
```

Check if the monitor timer is active:

```sh
systemctl status braid-monitor.timer
```

## How the pieces fit together

```
braid-monitor.timer (every 5 min)
  -> braid-monitor.service
    -> braid monitor (exit 0 = ok/offline/lock-contended, 1 = alert, 3 = advisory, 2 = setup error)
      -> on exit 1: start braid-alert.service
        -> starts braid-beep.service (PC speaker, 5s -> 10s -> ... -> 15min)
        -> alertCommand (if configured)
      -> on exit 3: start braid-alert-advisory.service
        -> alertCommand (if configured, no beep)

smartd (always running)
  -> detects SMART issue
  -> writes /var/lib/braid/smartd-alert flag
  -> starts braid-alert.service
  -> next braid monitor cycle picks up the flag

braid-scrub.service (scheduled scrub)
  -> fails to run/complete
  -> onFailure: braid-scrub-failed.service
    -> writes /var/lib/braid/scrub-failed flag
    -> starts braid-alert.service
    -> next braid monitor cycle picks up the flag

braid ack
  -> clears alert state
  -> stops braid-alert.service
    -> cascades to stop braid-beep.service
  -> stops braid-alert-advisory.service (Warning/ENOSPC tier, no beep)
```

## What's next

- [Power management](power-management.md) -- auto-suspend and Wake-on-LAN
- [Day-to-day NAS usage](day-to-day-nas-usage.md) -- good operator habits

## Related commands

- [monitor](../commands/monitor.md)
- [status](../commands/status.md)
- [ack](../commands/ack.md)
- [doctor](../commands/doctor.md)
