[← Manual](../index.md)

# braid ack

Acknowledges active alerts and silences the PC speaker beeper. When there is an active alert source on a mounted pool, it also sets the current device error counts as the new baseline so the same condition won't re-trigger.

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
   - If a latch entry exists, the smartd alert flag is present, or the latch is corrupt, snapshots the current `btrfs device stats` error counters and missing-device state.
   - Writes that snapshot as the new acknowledged baseline (`acked-stats.json`). Future monitor runs compare against this baseline, so the same error counts won't trigger again.
   - If none of those alert sources is present, exits 0 with `no active alerts` and does not query btrfs or rewrite `acked-stats.json`.
3. Stops `braid-alert.service` (the beeper), best-effort. This runs first so the stop attempt is reached before any later file-removal I/O error can short-circuit the rest of cleanup.
4. Removes the smartd alert flag (`smartd-alert`) if present.
5. Removes the alert latch file (`alert-latch.json`).
6. Removes the corrupt-latch sidecar (`alert-latch.json.corrupt`) if present.

If the pool is offline but alerts exist (e.g., a latched smartd alert), ack still clears the latch and flag without snapshotting device stats. Offline means there is no mount at the configured mount point. If that path is occupied by a non-btrfs filesystem, `braid ack` returns a probe error naming the fstype and preserves `alert-latch.json`, `smartd-alert`, and `acked-stats.json`.

## Flags

None.

## Safety checks

- If the pool is not mounted and no alerts are latched, ack refuses with "pool is not mounted -- nothing to acknowledge"
- If the pool is mounted but healthy with no latch entries, no smartd alert flag, and no corrupt latch, ack is a no-op and does not mutate `acked-stats.json`
- If the configured mount point is mounted as something other than btrfs, ack refuses with the fstype mismatch and does not clear or rewrite alert state

## Related commands

- [monitor](monitor.md) -- the automated check that triggers alerts
- [status](status.md) -- view active alerts
- [tui](tui.md) -- interactive dashboard shows alert state

## Related guides

- [Monitoring and alerts](../guides/monitoring-and-alerts.md)
