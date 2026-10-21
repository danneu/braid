---
experimental: true
---
[← braid](../index.md)

# braid ack

{{#include ../_includes/experimental-command-callout.md.inc}}

Acknowledges active alerts and silences the PC speaker beeper. It also records what you acknowledged so the same condition won't immediately re-trigger: the device error counts the monitor latched become the new baseline, a latched missing device the monitor still saw missing is marked acknowledged, and (on a mounted pool) a still-at-risk ENOSPC capacity warning is *snoozed* for a reminder interval -- a snooze, not a resolve, so `braid status` keeps showing the live capacity advisory.

Ack acknowledges only what the monitor latched -- what `braid status` showed you. It reads no live error counters or pool membership to build the baseline, so an error or a missing device that appears after the last monitor cycle, or counts that grew since then, still alert on the next cycle.

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
| **1** | Ack refused (see [Safety checks](#safety-checks)), or alert-file cleanup failed partway (step 7 below) |
| **2** | Setup error -- config could not be read, or pool-lock I/O error |

## What happens under the hood

Ack runs the same steps whether the pool is mounted or offline (nothing mounted at the configured mount point, for example because the pool is locked). The one exception is the ENOSPC snooze in step 6, which needs a mounted pool. See [ADR 014: Offline ack policy](../design/decisions/014-alerts.md#offline-ack-policy) for the rationale.

1. Reads the alert state once: the alert latch (`alert-latch.json`), the smartd alert flag (`smartd-alert`), the scrub-failed flag (`scrub-failed`), and the ack cleanup-pending sentinel (`alert-cleanup-pending`). Ack decides what to acknowledge and what to clear from this one snapshot; the stop in step 8 does not depend on it. A latch entry, either flag, or an unreadable latch is an *alert signal*; an unreadable latch carries no causes, but counting it as a signal lets ack clear it.
2. If the sentinel is set and there is no alert signal, an earlier ack failed partway through cleanup. Ack reruns the cleanup in step 7 directly, without probing the mount point or writing a baseline, then goes to step 8. A sentinel alongside an alert signal takes the full path below.
3. Probes the configured mount point. On a mounted btrfs pool this runs `btrfs filesystem show` and `cryptsetup status` for each member. A probe failure refuses the ack (see [Safety checks](#safety-checks)).
4. If there is no alert signal, there is nothing to acknowledge. Ack writes no `acked-stats.json`, changes no alert file, and goes to step 8.
5. Commits the latched evidence to the acknowledged baseline (`acked-stats.json`):
   - A latched btrfs device error sets that disk's baseline to the error counts the monitor latched. Future monitor runs compare against this baseline, so the same counts won't trigger again, but any count above it will.
   - A latched missing device that the latest monitor cycle still saw missing is marked acknowledged, so the next monitor cycle stays quiet while it stays missing.
   - Every other disk's baseline is left as it was. A smartd, scrub-failed, computation-error, or ENOSPC cause writes no baseline, and neither does an unreadable latch.
6. If the pool is mounted and the latch carries an `EnospcRisk` cause that is still at risk on a fresh `btrfs device usage` probe, writes a snooze marker (`enospc-ack.json`) with a reminder deadline one interval (7 days) out. If the pool has recovered by ack time, no marker is written, so a later recurrence alerts immediately. Offline, ack writes no marker, because it cannot probe the pool key or confirm the risk; a still-at-risk pool re-fires `EnospcRisk` -- a quiet Warning, no beep -- on remount and each later mounted cycle until a mounted ack snoozes it.
7. Cleans up the alert files. It first writes the sentinel `/var/lib/braid/alert-cleanup-pending`, then removes, in this order:
   - the smartd alert flag (`smartd-alert`), if step 1 saw it or the latch carried a smartd alert;
   - the scrub-failed flag (`scrub-failed`), by the same rule against a latched scrub failure;
   - the alert latch (`alert-latch.json`); a latched computation error re-fires on the next monitor cycle only if the underlying computation still fails;
   - the corrupt-latch sidecar (`alert-latch.json.corrupt`), if present.

   It deletes the sentinel only after every removal succeeds. A flag that appears after step 1, with no matching latched alert, is left for the next monitor cycle to latch.

   If a cleanup step fails, ack exits 1 and keeps the state the next `braid ack` needs to finish once you fix the I/O fault. A sentinel it already wrote stays: `braid status` surfaces ``ack cleanup pending -- re-run `braid ack` to resume`` as an alert cause until cleanup finishes, and the next ack resumes through step 2 (or the full path, if other signals remain). If the sentinel itself could not be written, nothing was removed, and the next ack starts over. See [ADR 014: Cleanup ordering and retry-on-failure](../design/decisions/014-alerts.md#cleanup-ordering-and-retry-on-failure).
8. Stops both alert units, best-effort: `braid-alert.service` (the Critical beeper -- that stop cascades through `BindsTo` to the `braid-beep.service` loop when beeping is enabled) and `braid-alert-advisory.service` (the non-beeping Warning advisory started on the proactive ENOSPC/capacity path). One ack silences whichever tier the last monitor cycle started. The stop runs on every ack that is not refused: a full ack, the retry in step 2, the no-op in step 4, and a failed cleanup. A unit can keep sounding with no alert record on disk -- for example when monitor could not write the latch -- and ack still silences it. Ack removes nothing after the stop, so a SMART or scrub alert that fires after the stop keeps its flag for the next monitor cycle. See [ADR 014: Ack always silences after its record work](../design/decisions/014-alerts.md#ack-always-silences-after-its-record-work).
9. Unless cleanup failed, prints the confirmation, after any stop warning: `no active alerts` after the no-op in step 4; otherwise the count of latched causes (`acknowledged 3 alerts`), or `acknowledged current alerts` when there is none to count -- only a flag, an unreadable latch, or the retry in step 2 (expected output, because only leftover cleanup ran).

**Warnings.** While committing the baseline in step 5, ack warns on stderr and keeps going in three cases, each of which makes an alert fire again rather than stay buried:

- `acked-stats.json` is corrupt (unparseable). Ack moves it aside to `acked-stats.json.corrupt` (an earlier sidecar is kept) and starts from an empty baseline, so previously acknowledged errors alert again. This also clears the monitor's computation error for that file.
- A latched btrfs device error carries no error counts (it was latched by an older braid, or a `braid add`, `remove`, `remove-missing`, or `replace` handed its devid to another disk or dropped it). Ack leaves that disk's baseline unchanged, so the alert fires again on the next cycle if the errors persist, this time with counts.
- A latched missing device is no longer known to be missing: a monitor cycle saw the devid back in the pool or gone from it, a `braid add`, `remove`, `remove-missing`, or `replace` handed the devid to another disk or dropped it, or an older braid latched it. Ack does not mark it acknowledged, so if that devid is missing on the next cycle -- the returned disk dropped out again, or the new disk did -- it alerts.

## Flags

None.

## Safety checks

Each refusal below exits 1. The alert units keep running, so the alarm still marks the alert ack did not handle. `alert-latch.json` (unreadable bytes included), `smartd-alert`, `scrub-failed`, and `acked-stats.json` stay as they were, with one exception: ack moves an unparseable `acked-stats.json` aside before it saves (see **Warnings**), so if that save fails, the file stays moved.

- If another braid operation holds the pool lock (`/run/braid-pool.lock`), waits up to 10 seconds for it to finish: proceeds if the lock frees within that window, otherwise refuses with the pool-lock retry message.
- If the configured mount point holds a filesystem other than btrfs, refuses with a probe error naming the fstype, even when there is nothing to acknowledge. Any other probe failure in step 3 refuses the same way. Only the cleanup-only retry (step 2) runs before this probe.
- If `acked-stats.json` cannot be read or saved (a permission or disk error; unparseable content is a warning instead, see **Warnings**), refuses. Ack cannot tell what an unreadable file holds, so it keeps refusing until you fix or remove the file by hand.

## Related commands

- [monitor](monitor.md) -- the automated check that triggers alerts
- [status](status.md) -- view active alerts
- [tui](tui.md) -- interactive dashboard shows alert state

## Related guides

- [Monitoring and alerts](../guides/monitoring-and-alerts.md)
