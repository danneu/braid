---
experimental: true
---
[← braid](../index.md)

# braid monitor

> [!NOTE]
> This command is experimental. It is less-trodden and more likely to have rough edges while braid is pre-v1.0.

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
| **0** | Healthy, pool is offline, or another braid command holds the pool lock (cycle skipped, re-evaluated on the next timer tick) |
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
braid monitor      --writes--> alert-latch.json --> braid status / braid tui (display)
(timer, every 5m)  --exit 1--> braid-alert.service (beeper + alertCommand)

smartd  --start-->  braid-alert.service (beeper)
        --writes--> smartd-alert --> next braid monitor cycle (latches SmartdAlert)
```

On exit 1, the `braid-monitor.service` wrapper starts `braid-alert.service` (the beeper, plus any `alertCommand`). After that, two things stay active until you `braid ack`, each held by a different mechanism:

- **The latch and exit 1** -- held by **monitor**. Each cycle it writes the live causes to `alert-latch.json`, merging them into the existing latch, and re-exits 1 while any cause remains. `braid status` and the TUI read the same file for display.
- **The beep** -- held by **`braid-alert.service` itself**, not the read-back. Once started it stays active on its own (the backoff beep loop when beep is enabled, or a `RemainAfterExit` oneshot when it's off), so the wrapper's per-cycle `systemctl start` is a no-op and a skipped cycle (offline or lock-contended exit 0) does not silence it. The service never reads `alert-latch.json` or the `smartd-alert` flag.

`smartd` is a second, independent trigger: on a SMART fault it starts `braid-alert.service` directly *and* writes the `smartd-alert` flag, which the next monitor cycle latches as a `SmartdAlert` cause.

The beep stops only when `braid ack` clears the latch and runs `systemctl stop braid-alert.service`.

## Related commands

- [ack](ack.md) -- acknowledge alerts and silence the beeper
- [doctor](doctor.md) -- one-time diagnostic; pass `--beep` to test the alert beep
- [status](status.md) -- shows active alerts in the status output

## Related guides

- [Monitoring and alerts](../guides/monitoring-and-alerts.md)
