[← Manual](../index.md)

# braid ack

Acknowledges active alerts, silences the PC speaker beeper, and sets the current device error counts as the new baseline so the same condition won't re-trigger.

## When to use it

- The beeper is going off and you've investigated the cause.
- `braid status` or `braid tui` shows active alerts you've already addressed.
- After replacing a disk or running a scrub to clear errors.

## Basic example

```
sudo braid ack
```

Output:

```
acknowledged 3 alert(s)
```

If there's nothing to acknowledge:

```
no active alerts
```

## What happens under the hood

1. Reads the alert latch to determine how many alerts are active.
2. If the pool is mounted:
   - Snapshots the current `btrfs device stats` error counters and missing-device state.
   - Writes the snapshot as the new acknowledged baseline (`acked-stats.json`). Future monitor runs compare against this baseline, so the same error counts won't trigger again.
3. Removes the alert latch file (`alert-latch.json`).
4. Removes the smartd alert flag if present.
5. Stops `braid-alert.service` (the beeper), best-effort.

If the pool is offline but alerts exist (e.g., a latched smartd alert), ack still clears the latch and flag without snapshotting device stats.

## Flags

None.

## Safety checks

- If the pool is not mounted and no alerts are latched, ack refuses with "pool is not mounted -- nothing to acknowledge"

## Related commands

- [monitor](monitor.md) -- the automated check that triggers alerts
- [status](status.md) -- view active alerts
- [tui](tui.md) -- interactive dashboard shows alert state

## Related guides

- [Monitoring and alerts](../guides/monitoring-and-alerts.md)
