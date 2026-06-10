---
experimental: true
---
[← braid](../index.md)

# braid ack

{{#include ../_includes/experimental-command-callout.md.inc}}

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
acknowledged 3 alerts
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

On a cleanup I/O error, ack preserves retry state so the next `braid ack` resumes cleanup after the I/O fault is fixed.

When ack reaches cleanup and a later cleanup step fails, it leaves `/var/lib/braid/alert-cleanup-pending`. `braid status` surfaces ``ack cleanup pending -- re-run `braid ack` to resume`` as an alert cause until cleanup finishes. If that sentinel is the only remaining alert signal, the next `braid ack` re-enters cleanup directly (no btrfs probe, no baseline rewrite) and prints `acknowledged current alerts` on success -- expected output because only leftover cleanup ran.

When the pool is offline (no mount at the configured mount point), `braid ack` cannot run `btrfs device stats`, so what it can clear depends on which alert signals are present:

- A smartd alert -- a latched smartd cause, a bare `smartd-alert` flag present at ack entry, or both -- clears any latch and removes the `smartd-alert` flag; no `acked-stats.json` write is needed.
- A latched computation error clears the latch; it re-fires on the next monitor cycle only if the underlying computation still fails.
- A latched missing device is recorded as acknowledged in `acked-stats.json` (so the next monitor cycle stays quiet) and the latch is cleared, without querying btrfs.
- A latched btrfs device error is refused: ack exits non-zero with `cannot ack btrfs device errors while pool is offline -- unlock the pool first` and leaves all alert state untouched, because re-baselining the error counters needs live `btrfs device stats`, which requires the pool mounted. The refusal is all-or-nothing -- a co-latched missing device is not partially acknowledged, so unlock and re-run to clear everything.

If that mount point is occupied by a non-btrfs filesystem, `braid ack` returns a probe error naming the fstype and preserves `alert-latch.json`, `smartd-alert`, and `acked-stats.json`.

See [ADR 014: Offline ack policy](../design/decisions/014-alerts.md#offline-ack-policy) for the rationale.

## Flags

None.

## Safety checks

- If the pool is offline and no alert signal is present -- no latch entries, no smartd alert flag, no corrupt latch, and no pending ack cleanup -- ack refuses with "pool is not mounted -- nothing to acknowledge"
- If the pool is offline and any latched cause is a btrfs device error, ack refuses with "cannot ack btrfs device errors while pool is offline -- unlock the pool first" and leaves all alert state untouched (a co-latched missing device is not partially acknowledged).
- If the pool is mounted but healthy with no latch entries, no smartd alert flag, and no corrupt latch, ack is a no-op and does not mutate `acked-stats.json`
- If the configured mount point is mounted as something other than btrfs, ack refuses with the fstype mismatch and does not clear or rewrite alert state
- If another braid operation holds the pool lock (`/run/braid-pool.lock`), waits up to 10 seconds for it to finish: proceeds if the lock frees within that window, otherwise exits 1 with the pool-lock retry message.

## Related commands

- [monitor](monitor.md) -- the automated check that triggers alerts
- [status](status.md) -- view active alerts
- [tui](tui.md) -- interactive dashboard shows alert state

## Related guides

- [Monitoring and alerts](../guides/monitoring-and-alerts.md)
