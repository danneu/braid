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

- **btrfs device errors** -- any device in the pool has read, write, flush, corruption, or generation errors above the acknowledged baseline, including errors discovered during scrub.
- **Missing device** -- btrfs reports a device as missing or a pool device has a null underlying path.
- **SMART alert** -- smartd has written a SMART alert flag (via the braid smartd notifier).
- **Computation error** -- a probe, parse, btrfs device stats call, mountinfo read, acked-stats baseline load, acked-stats save during self-heal, or alert-latch load/quarantine failed. Monitor fails closed: it latches a `ComputationError` cause so the beeper fires and `braid status` shows the detail.

## Flags

None. Monitor has no flags -- it reads from the braid config and state files.

## What happens under the hood

1. Checks if the pool is mounted. If not, exits 0 (nothing to monitor).
2. Runs `btrfs device stats` on the pool mount point.
3. Loads the acknowledged-stats baseline (`acked-stats.json`) from a previous `braid ack`. If the file is unreadable or unparseable, monitor fails closed -- it latches a `ComputationError` rather than firing every acknowledged cause against an empty baseline.
4. Self-heals stale ack state before computing alerts: prunes baseline entries for devices no longer in the pool, and clears the missing-acked flag for any device that was acknowledged missing but is now present again. If the baseline changed, the updated `acked-stats.json` is written immediately; a write failure (e.g. EROFS, ENOSPC) is itself a fail-closed `ComputationError`.
5. Computes alert causes against the reconciled baseline: btrfs device errors above the baseline, missing/null-underlying devices, and the smartd alert flag.
6. Merges the causes into the alert latch (`alert-latch.json`). The latch is sticky: once an alert fires, it stays active until `braid ack` clears it.

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
