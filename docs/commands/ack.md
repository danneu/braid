---
experimental: true
---
[← braid](../index.md)

# braid ack

{{#include ../_includes/experimental-command-callout.md.inc}}

Acknowledges active alerts and silences the PC speaker beeper. It also records what you acknowledged so the same condition won't immediately re-trigger: the device error counts the monitor latched become the new baseline, a latched missing device the monitor still saw missing is marked acknowledged, and (on a mounted pool) a still-at-risk ENOSPC capacity warning is *snoozed* for a reminder interval -- a snooze, not a resolve, so `braid status` keeps showing the live capacity advisory.

Ack acknowledges only what the monitor latched -- what `braid status` showed you. It reads no live error counters, so errors that appear after the last monitor cycle, or counts that grew since then, still alert on the next cycle.

## When to use it

- The beeper is going off and you've investigated the cause.
- `braid status` or `braid tui` shows active alerts you've already addressed.
- After replacing a disk or running a scrub to clear errors.

## Basic example

```
sudo braid ack
```

Output, whether the pool is mounted or offline:

```
acknowledged 3 alerts
```

If there's nothing to acknowledge, mounted or offline, ack exits 0 with:

```
no active alerts
```

Ack still stops the alert units in that case, so a beeper that is sounding with no recorded alert goes quiet.

## Exit codes

| Exit code | Meaning |
|---|---|
| **0** | Alerts acknowledged, or nothing to acknowledge |
| **1** | Lock contention (retry once the other operation finishes), or an ack failure (probe/fstype error, state-file I/O, cleanup I/O) |
| **2** | Setup error -- config could not be read, or pool-lock I/O error |

## What happens under the hood

1. Reads the alert latch to determine how many alerts are active.
2. If no alert source is present -- no latch entry, no smartd alert flag, no scrub-failed flag, no corrupt latch, and no pending ack cleanup -- skips to step 9 and prints `no active alerts` (exit 0), whether the pool is mounted or offline. It does not rewrite `acked-stats.json`.
3. Commits the latched evidence to the acknowledged baseline (`acked-stats.json`), mounted or offline alike:
   - A latched btrfs device error sets that disk's baseline to the error counts the monitor latched. Future monitor runs compare against this baseline, so the same counts won't trigger again, but any count above it will.
   - A latched missing device that the latest monitor cycle still saw missing is marked acknowledged, so the next monitor cycle stays quiet while it stays missing.
   - Every other disk's baseline is left as it was. A smartd, scrub-failed, computation-error, or ENOSPC cause writes no baseline, and neither does a corrupt latch.
4. If the pool is mounted and a latched `EnospcRisk` is still at risk on a fresh `btrfs device usage` probe, writes a snooze marker (`enospc-ack.json`) with a reminder deadline one interval (7 days) out. This *snoozes* the monitor reminder -- it does not resolve the risk, and `braid status` keeps showing the live advisory. If the pool has recovered by ack time, no marker is written, so a later recurrence alerts immediately.
5. Removes the smartd alert flag (`smartd-alert`) if present.
6. Removes the scrub-failed flag (`scrub-failed`) if present.
7. Removes the alert latch file (`alert-latch.json`).
8. Removes the corrupt-latch sidecar (`alert-latch.json.corrupt`) if present.
9. Stops both alert units, best-effort: `braid-alert.service` (the Critical beeper -- that stop cascades through `BindsTo` to the `braid-beep.service` loop when beeping is enabled) and `braid-alert-advisory.service` (the non-beeping Warning advisory started on the proactive ENOSPC/capacity path). One ack silences whichever tier the last monitor cycle started.

The stop in step 9 runs on every ack that is not refused, even when there was nothing to acknowledge or a removal failed. A unit can keep sounding with no alert record on disk -- for example when monitor could not write the latch -- and ack still silences it. The stop runs after the removals, so a SMART or scrub alert that fires after the stop keeps its flag for the next monitor cycle. A refused ack leaves the units running: a probe or fstype error, or an I/O error before cleanup.

**Offline ack.** When the pool is locked or unmounted, ack commits the same latched evidence as a mounted ack and clears the same files. Only the ENOSPC snooze needs the live pool: an `EnospcRisk` ack still clears the latch but writes no snooze marker (offline cannot probe the pool key or confirm risk). A still-at-risk pool re-fires `EnospcRisk` -- a quiet Warning, no beep -- on remount and each subsequent mounted cycle until a mounted ack snoozes it.

**Warnings.** Ack warns on stderr and keeps going in three cases, each of which makes an alert fire again rather than stay buried:

- `acked-stats.json` is corrupt (unparseable). Ack moves it aside to `acked-stats.json.corrupt` (an earlier sidecar is kept) and starts from an empty baseline, so previously acknowledged errors alert again. This also clears the monitor's computation error for that file.
- A latched btrfs device error carries no error counts (it was latched by an older braid, or a `braid add`, `remove`, `remove-missing`, or `replace` handed its devid to another disk or dropped it). Ack leaves that disk's baseline unchanged, so the alert fires again on the next cycle if the errors persist, this time with counts.
- A latched missing device is no longer known to be missing: a monitor cycle saw the devid back in the pool or gone from it, a `braid add`, `remove`, `remove-missing`, or `replace` handed the devid to another disk or dropped it, or an older braid latched it. Ack does not mark it acknowledged, so if that devid is missing on the next cycle -- the returned disk dropped out again, or the new disk did -- it alerts.

A read error on `acked-stats.json` (a permission or disk error) is not a warning case. Ack cannot tell what the file holds, so it refuses: it exits 1, keeps the latch, and leaves the alert units running until you fix or remove the file by hand.

On a cleanup I/O error, ack preserves retry state so the next `braid ack` resumes cleanup after the I/O fault is fixed.

When ack reaches cleanup and a later cleanup step fails, it leaves `/var/lib/braid/alert-cleanup-pending`. `braid status` surfaces ``ack cleanup pending -- re-run `braid ack` to resume`` as an alert cause until cleanup finishes. If that sentinel is the only remaining alert signal, the next `braid ack` re-enters cleanup directly (no btrfs probe, no baseline rewrite) and prints `acknowledged current alerts` on success -- expected output because only leftover cleanup ran.

When the pool is offline (no mount at the configured mount point), each alert signal clears the same way it does on a mounted pool:

- A smartd alert -- a latched smartd cause, a bare `smartd-alert` flag present at ack entry, or both -- clears any latch and removes the `smartd-alert` flag; no `acked-stats.json` write is needed.
- A scrub failure -- a latched `ScrubFailed` cause, a bare `scrub-failed` flag present at ack entry, or both -- clears any latch and removes the `scrub-failed` flag (mirroring the smartd source); no `acked-stats.json` write is needed.
- A latched computation error clears the latch; it re-fires on the next monitor cycle only if the underlying computation still fails.
- A latched missing device that the latest monitor cycle still saw missing is recorded as acknowledged in `acked-stats.json` (so the next monitor cycle stays quiet) and the latch is cleared.
- A latched btrfs device error commits the counts the monitor latched as that disk's baseline and the latch is cleared.

If that mount point is occupied by a non-btrfs filesystem, `braid ack` returns a probe error naming the fstype, even when there is nothing to acknowledge, and preserves `alert-latch.json`, `smartd-alert`, `scrub-failed`, and `acked-stats.json`.

See [ADR 014: Offline ack policy](../design/decisions/014-alerts.md#offline-ack-policy) for the rationale.

## Flags

None.

## Safety checks

- If no alert signal is present -- no latch entries, no smartd alert flag, no scrub-failed flag, no corrupt latch, and no pending ack cleanup -- ack is a no-op whether the pool is mounted or offline: it prints `no active alerts`, exits 0, and does not mutate `acked-stats.json` or any other alert file. It still stops the alert units.
- Ack never reads live error counters or pool membership to build the baseline, so it cannot acknowledge an error or a missing device the monitor has not latched
- If the configured mount point is mounted as something other than btrfs, ack refuses with the fstype mismatch, does not clear or rewrite alert state, and leaves the alert units running
- If another braid operation holds the pool lock (`/run/braid-pool.lock`), waits up to 10 seconds for it to finish: proceeds if the lock frees within that window, otherwise exits 1 with the pool-lock retry message.

## Related commands

- [monitor](monitor.md) -- the automated check that triggers alerts
- [status](status.md) -- view active alerts
- [tui](tui.md) -- interactive dashboard shows alert state

## Related guides

- [Monitoring and alerts](../guides/monitoring-and-alerts.md)
