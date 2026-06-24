---
experimental: true
---
[← braid](../index.md)

# braid ack

{{#include ../_includes/experimental-command-callout.md.inc}}

Acknowledges active alerts and silences the PC speaker beeper. On a mounted pool it also records the acknowledged state so the same condition won't immediately re-trigger: device error counts become the new baseline, and a still-at-risk ENOSPC capacity warning is *snoozed* for a reminder interval -- a snooze, not a resolve, so `braid status` keeps showing the live capacity advisory.

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

## Exit codes

| Exit code | Meaning |
|---|---|
| **0** | Alerts acknowledged, or nothing to acknowledge |
| **1** | Lock contention (retry once the other operation finishes), or an ack failure (offline btrfs-error refusal, probe/fstype error, cleanup I/O) |
| **2** | Setup error -- config could not be read, or pool-lock I/O error |

## What happens under the hood

1. Reads the alert latch to determine how many alerts are active.
2. If the pool is mounted:
   - If a latch entry exists, the smartd alert flag is present, the scrub-failed flag is present, or the latch is corrupt, snapshots the current `btrfs device stats` error counters and missing-device state.
   - Writes that snapshot as the new acknowledged baseline (`acked-stats.json`). Future monitor runs compare against this baseline, so the same error counts won't trigger again.
   - If a latched `EnospcRisk` is still at risk on a fresh `btrfs device usage` probe, writes a snooze marker (`enospc-ack.json`) with a reminder deadline one interval (7 days) out. This *snoozes* the monitor reminder -- it does not resolve the risk, and `braid status` keeps showing the live advisory. If the pool has recovered by ack time, no marker is written, so a later recurrence alerts immediately.
   - If none of those alert sources is present, exits 0 with `no active alerts` and does not query btrfs or rewrite `acked-stats.json`.
3. Stops both alert units, best-effort: `braid-alert.service` (the Critical beeper -- that stop cascades through `BindsTo` to the `braid-beep.service` loop when beeping is enabled) and `braid-alert-advisory.service` (the non-beeping Warning advisory started on the proactive ENOSPC/capacity path). One ack silences whichever tier the last monitor cycle started. This runs first so the stop attempt is reached before any later file-removal I/O error can short-circuit the rest of cleanup.
4. Removes the smartd alert flag (`smartd-alert`) if present.
5. Removes the scrub-failed flag (`scrub-failed`) if present.
6. Removes the alert latch file (`alert-latch.json`).
7. Removes the corrupt-latch sidecar (`alert-latch.json.corrupt`) if present.

**Offline ack.** When the pool is locked or unmounted, an `EnospcRisk` ack still clears the latch but writes no snooze marker (offline cannot probe the pool key or confirm risk). A still-at-risk pool re-fires `EnospcRisk` -- a quiet Warning, no beep -- on remount and each subsequent mounted cycle until a mounted ack snoozes it.

On a cleanup I/O error, ack preserves retry state so the next `braid ack` resumes cleanup after the I/O fault is fixed.

When ack reaches cleanup and a later cleanup step fails, it leaves `/var/lib/braid/alert-cleanup-pending`. `braid status` surfaces ``ack cleanup pending -- re-run `braid ack` to resume`` as an alert cause until cleanup finishes. If that sentinel is the only remaining alert signal, the next `braid ack` re-enters cleanup directly (no btrfs probe, no baseline rewrite) and prints `acknowledged current alerts` on success -- expected output because only leftover cleanup ran.

When the pool is offline (no mount at the configured mount point), `braid ack` cannot run `btrfs device stats`, so what it can clear depends on which alert signals are present:

- A smartd alert -- a latched smartd cause, a bare `smartd-alert` flag present at ack entry, or both -- clears any latch and removes the `smartd-alert` flag; no `acked-stats.json` write is needed.
- A scrub failure -- a latched `ScrubFailed` cause, a bare `scrub-failed` flag present at ack entry, or both -- clears any latch and removes the `scrub-failed` flag (mirroring the smartd source); no `acked-stats.json` write is needed.
- A latched computation error clears the latch; it re-fires on the next monitor cycle only if the underlying computation still fails.
- A latched missing device is recorded as acknowledged in `acked-stats.json` (so the next monitor cycle stays quiet) and the latch is cleared, without querying btrfs.
- A latched btrfs device error is refused: ack exits 1 with `cannot ack btrfs device errors while pool is offline -- unlock the pool first` and leaves all alert state untouched, because re-baselining the error counters needs live `btrfs device stats`, which requires the pool mounted. The refusal is all-or-nothing -- a co-latched missing device is not partially acknowledged, so unlock and re-run to clear everything.

If that mount point is occupied by a non-btrfs filesystem, `braid ack` returns a probe error naming the fstype and preserves `alert-latch.json`, `smartd-alert`, `scrub-failed`, and `acked-stats.json`.

See [ADR 014: Offline ack policy](../design/decisions/014-alerts.md#offline-ack-policy) for the rationale.

## Flags

None.

## Safety checks

- If the pool is offline and no alert signal is present -- no latch entries, no smartd alert flag, no scrub-failed flag, no corrupt latch, and no pending ack cleanup -- ack refuses with "pool is not mounted -- nothing to acknowledge"
- If the pool is offline and any latched cause is a btrfs device error, ack refuses with "cannot ack btrfs device errors while pool is offline -- unlock the pool first" and leaves all alert state untouched (a co-latched missing device is not partially acknowledged).
- If the pool is mounted but healthy with no latch entries, no smartd alert flag, no scrub-failed flag, and no corrupt latch, ack is a no-op and does not mutate `acked-stats.json`
- If the configured mount point is mounted as something other than btrfs, ack refuses with the fstype mismatch and does not clear or rewrite alert state
- If another braid operation holds the pool lock (`/run/braid-pool.lock`), waits up to 10 seconds for it to finish: proceeds if the lock frees within that window, otherwise exits 1 with the pool-lock retry message.

## Related commands

- [monitor](monitor.md) -- the automated check that triggers alerts
- [status](status.md) -- view active alerts
- [tui](tui.md) -- interactive dashboard shows alert state

## Related guides

- [Monitoring and alerts](../guides/monitoring-and-alerts.md)
