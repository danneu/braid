---
experimental: true
---
[← braid](../index.md)

# braid monitor

{{#include ../_includes/experimental-command-callout.md.inc}}

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
| **1** | Critical alert active -- a disk-health problem; the beeper fires |
| **3** | Warning-only alert active -- a proactive capacity (ENOSPC) risk; notifies via `alertCommand`, no beep |
| **2** | Pre-monitor setup error (e.g. pool-lock I/O, config load failure) |

## What triggers an alert

Alerts have two severities. The audible beep is reserved for Critical; a Warning-only cycle takes the non-beeping advisory path.

**Critical (exit 1, beeps):**

- **btrfs device errors** -- any device in the pool has read, write, flush, corruption, or generation errors above the acknowledged baseline, including errors discovered during scrub.
- **Missing device** -- btrfs reports a device as missing or a pool device has a null underlying path.
- **SMART alert** -- smartd has written a SMART alert flag (via the braid smartd notifier).
- **Scrub failed** -- the scheduled maintenance scrub failed to run or complete. `braid-scrub.service`'s `onFailure` writes a `scrub-failed` flag and starts the beeper; the next monitor cycle latches a `ScrubFailed` cause. This covers scrub *execution* failure only -- corruption *found* by a scrub still alerts as **btrfs device errors** above.
- **Computation error** -- a probe, parse, btrfs device stats call, mountinfo read, acked-stats baseline load, acked-stats save during self-heal, or alert-latch load/quarantine failed. Monitor fails closed: it latches a `ComputationError` cause so the beeper fires and `braid status` shows the detail.

**Warning (exit 3, no beep -- notifies via `alertCommand` and `braid status`):**

- **ENOSPC risk** -- the pool is one disk-loss away from being unable to allocate the RAID1 chunk pairs needed to restore redundancy. This is the same shared predicate `braid status` and `braid doctor` report, now evaluated proactively each monitor cycle. Acknowledge to **snooze** the reminder (default 7 days), not resolve it: if the pool is still at risk when the interval elapses, monitor reminds again (and `braid status` shows the advisory the whole time); ack again to re-snooze. It re-arms immediately when the risk clears. Best-effort: if the `btrfs device usage` probe fails, only this check is skipped -- it never masks a Critical alert in the same cycle, and never escalates to a beep.

## Flags

None. Monitor has no flags -- it reads from the braid config and state files.

## What happens under the hood

1. Checks if the pool is mounted. If not, exits 0 (nothing to monitor).
2. Runs `btrfs device stats` on the pool mount point.
3. Loads the acknowledged-stats baseline (`acked-stats.json`) from a previous `braid ack`. If the file is unreadable or unparseable, monitor fails closed -- it latches a `ComputationError` rather than firing every acknowledged cause against an empty baseline.
4. Self-heals stale ack state before computing alerts: prunes baseline entries for devices no longer in the pool, and clears the missing-acked flag for any device that was acknowledged missing but is now present again. If the baseline changed, the updated `acked-stats.json` is written immediately; a write failure (e.g. EROFS, ENOSPC) is itself a fail-closed `ComputationError`.
5. Computes alert causes against the reconciled baseline: btrfs device errors above the baseline, missing/null-underlying devices, the smartd alert flag, and the scrub-failed flag.
6. Best-effort ENOSPC check: probes `btrfs device usage` and raises an `EnospcRisk` Warning when the pool is one disk-loss from RAID1 chunk-pair exhaustion. A matching `enospc-ack.json` snooze marker suppresses it until its reminder deadline elapses, after which it re-fires every cycle until a re-ack; the marker is dropped (re-armed) when the risk clears. A probe or parse failure skips only this check.
7. Merges the causes into the alert latch (`alert-latch.json`). The latch is sticky: once an alert fires, it stays active until `braid ack` clears it.

## Alert pipeline

```
braid monitor      --writes--> alert-latch.json --> braid status / braid tui (display)
(timer, every 5m)  --exit 1--> braid-alert.service (latched orchestrator + alertCommand)
                                  --wants--> braid-beep.service (backoff beep loop)
                   --exit 3--> braid-alert-advisory.service (alertCommand only, no beep)

smartd  --start-->  braid-alert.service
        --writes--> smartd-alert --> next braid monitor cycle (latches SmartdAlert)

braid-scrub.service --onFailure--> braid-scrub-failed.service
        --start-->  braid-alert.service
        --writes--> scrub-failed --> next braid monitor cycle (latches ScrubFailed)
```

On exit 1, the `braid-monitor.service` wrapper starts `braid-alert.service`, a latched orchestrator that runs any `alertCommand` and pulls in `braid-beep.service` when beeping is enabled. On exit 3 it starts `braid-alert-advisory.service`, which runs only `alertCommand` (no beep). After that, two things stay active until you `braid ack`, each held by a different mechanism:

- **The latch and exit 1** -- held by **monitor**. Each cycle it writes the live causes to `alert-latch.json`, merging them into the existing latch, and re-exits 1 while any cause remains. `braid status` and the TUI read the same file for display.
- **The beep** -- held by **`braid-beep.service`**, not the read-back. `braid-alert.service` remains active as a `RemainAfterExit` latch, while the beep loop stays active in the bound service. The wrapper's per-cycle `systemctl start` is a no-op and a skipped cycle (offline or lock-contended exit 0) does not silence it. Neither service reads `alert-latch.json` or the `smartd-alert` flag.

`smartd` is a second, independent trigger: on a SMART fault it starts `braid-alert.service` directly *and* writes the `smartd-alert` flag, which the next monitor cycle latches as a `SmartdAlert` cause.

A failed scheduled scrub is a third, independent trigger with the same shape: `braid-scrub.service`'s `onFailure` runs `braid-scrub-failed.service`, which starts `braid-alert.service` directly *and* writes the `scrub-failed` flag, which the next monitor cycle latches as a `ScrubFailed` cause. A deliberate cancel (lock/suspend/shutdown) and a corruption-found scrub (btrfs exit 3) are both successes, so neither fires this path. This whole path exists only when `braid.monitor` is enabled.

The beep stops only when `braid ack` clears the latch and runs `systemctl stop braid-alert.service`; that stop cascades to `braid-beep.service` through `BindsTo`. The same ack also stops `braid-alert-advisory.service`, so a Warning-tier advisory is silenced too.

## Related commands

- [ack](ack.md) -- acknowledge alerts and silence the beeper
- [doctor](doctor.md) -- one-time diagnostic; pass `--beep` to test the alert beep
- [status](status.md) -- shows active alerts in the status output

## Related guides

- [Monitoring and alerts](../guides/monitoring-and-alerts.md)
