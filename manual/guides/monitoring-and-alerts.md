[← Manual](../index.md)

# Monitoring and alerts

This guide covers how braid monitors disk health and notifies you when something goes wrong.

Read this if you want to understand the alert system, configure notifications, or respond to an alert.

## How monitoring works

braid runs a health check every 5 minutes via a systemd timer. The check looks at three things:

1. **btrfs device stats** -- non-zero error counters (read, write, flush, corruption, generation errors) on any drive.
2. **Missing devices** -- a drive that should be in the pool but is not present.
3. **SMART alerts** -- smartd detected a SMART health warning on a drive.

If any check triggers, braid activates an alert.

### What happens on alert

When `braid monitor` detects an issue (exit code 1), the systemd wrapper starts `braid-alert.service`, which:

- **Beeps the PC speaker** every 15 seconds (if enabled) until acknowledged.
- **Runs your custom alert command** (if configured).

The beeping is intentionally persistent and annoying -- you should not be able to ignore a disk problem on a NAS that holds your data.

### Alerts are latched

An alert stays active until you acknowledge it with `braid ack`, even if the triggering condition goes away. This is by design: "a disk had errors" is worth investigating even if the error count stopped growing.

## Configuration

Monitoring is on by default when `braid.enable = true`. Here is the full set of options:

```nix
braid = {
  enable = true;
  package = braid.packages.x86_64-linux.default;

  monitor = {
    enable = true;        # default: true
    interval = "5min";    # default: "5min" (systemd time span)
    beep = true;          # default: true (PC speaker alert)
    alertCommand = null;  # default: null (optional custom command)
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

### Custom alert commands

Set `monitor.alertCommand` to run a script when an alert fires. This runs in addition to (not instead of) the beep:

```nix
braid.monitor.alertCommand = "/home/user/scripts/send-pushover-alert.sh";
```

The command runs as root. It should be idempotent -- it may fire on every monitor cycle while the alert is active.

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

This silences the beep and resets the alert baseline. New errors after ack will trigger a fresh alert.

## Checking monitor status

View the monitor service logs:

```sh
journalctl -u braid-monitor.service --since "1 hour ago"
```

View the alert service:

```sh
journalctl -u braid-alert.service
```

Check if the monitor timer is active:

```sh
systemctl status braid-monitor.timer
```

## How the pieces fit together

```
braid-monitor.timer (every 5 min)
  -> braid-monitor.service
    -> braid monitor (exit 0 = ok, 1 = alert, 2 = error)
      -> on exit 1: start braid-alert.service
        -> beep (PC speaker, every 15s)
        -> alertCommand (if configured)

smartd (always running)
  -> detects SMART issue
  -> writes /var/lib/braid/smartd-alert flag
  -> starts braid-alert.service
  -> next braid monitor cycle picks up the flag

braid ack
  -> clears alert state
  -> braid-alert.service stops (beeping stops)
```

## What's next

- [Power management](power-management.md) -- auto-suspend and Wake-on-LAN
- [Day-to-day NAS usage](day-to-day-nas-usage.md) -- good operator habits

## Related commands

- [monitor](../commands/monitor.md)
- [status](../commands/status.md)
- [ack](../commands/ack.md)
- [doctor](../commands/doctor.md)
