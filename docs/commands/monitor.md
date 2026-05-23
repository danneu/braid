[← braid](../index.md)

# braid monitor

Checks btrfs device error stats, missing devices, and SMART alerts. Designed to be run automatically by a systemd timer (every 5 minutes by default). Exits with a status code that drives the alert pipeline.

## When to use it

You normally don't run this by hand -- the `braid-monitor.timer` systemd unit runs it automatically. Use it directly when debugging the alert system or testing your monitoring setup.

## Basic example

```
sudo braid monitor
```

No output on success. Check the exit code:

```
sudo braid monitor; echo $?
```

## Exit codes

| Code | Meaning |
| --- | --- |
| **0** | Healthy, or pool is offline (nothing to check) |
| **1** | Alert active -- one or more problems detected |
| **2** | Pre-monitor setup error (e.g. pool-lock I/O, config load failure) |

## What triggers an alert (exit 1)

- **btrfs device errors** -- any device in the pool has read, write, flush, corruption, or generation errors above the acknowledged baseline.
- **Missing device** -- btrfs reports a device as missing or a pool device has a null underlying path.
- **SMART alert** -- smartd has written a SMART alert flag (via the braid smartd notifier).
- **Computation error** -- a probe, parse, btrfs device stats call, mountinfo read, acked-stats baseline read, or alert latch read failed. Monitor fails closed: it latches a `ComputationError` cause so the beeper fires and `braid status` shows the detail.

## Flags

None. Monitor has no flags -- it reads from the braid config and state files.

## What happens under the hood

1. Checks if the pool is mounted. If not, exits 0 (nothing to monitor).
2. Runs `btrfs device stats` on the pool mount point.
3. Loads the acknowledged-stats baseline (`acked-stats.json`) from a previous `braid ack`.
4. Computes which devices have new errors above the baseline.
5. Checks for missing/null-underlying devices.
6. Checks for a smartd alert flag.
7. Merges results into the alert latch (`alert-latch.json`). The latch is sticky: once an alert fires, it stays active until `braid ack` clears it.
8. Self-heals stale ack state: if a device was previously acknowledged as missing but is now present, the missing-acked flag is automatically cleared.

## Alert pipeline

```
braid monitor (timer) --> alert-latch.json --> braid-alert.service (beeper)
                                           --> braid status / braid tui (display)
```

When monitor writes an active alert latch, the systemd alert service activates the PC speaker beeper. The alert stays latched until you run `braid ack`.

## Related commands

- [ack](ack.md) -- acknowledge alerts and silence the beeper
- [doctor](doctor.md) -- one-time diagnostic; pass `--beep` to test the alert beep
- [status](status.md) -- shows active alerts in the status output

## Related guides

- [Monitoring and alerts](../guides/monitoring-and-alerts.md)
