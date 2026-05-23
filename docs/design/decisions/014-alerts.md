---
intent: Document braid's first-class alert model, alert latching, acknowledgment state, and monitor/ack behavior. Read before modifying alert computation, monitor, status, TUI alert display, or ack semantics.
status: Active
---

# First-Class Alerts for Disk Health


## Context

Synology NAS boxes beep when a disk develops bad sectors — you hear it, SSH in, and deal with it. Without active alerting, a braid NAS user has no idea anything is wrong unless they happen to run `braid status`.

## Decision

### Alert as primary domain concept

braid has first-class Alerts. An Alert represents "something happened that needs human acknowledgment." Beeping is one notification mechanism for an active alert. `braid status` is the primary surface for understanding alert details. `braid ack` acknowledges current alerts and silences notifications.

### Shared alert computation

A single shared computation produces an `AlertState` consumed by all surfaces — `braid monitor` (exit code), `braid status` (banner + causes), TUI (banner + indicators). No surface re-encodes alert logic.

### Alert causes

`AlertCause` is an explicit enum:
- `BtrfsDeviceErrors { devid }` — non-zero btrfs device stat counters above acked baseline
- `MissingDevice { devid }` — device missing from pool
- `SmartdAlert` — smartd SMART health warning
- `ComputationError { detail }` — probe or parse failed before a structured cause could be determined

The status banner is cause-neutral ("disk health issue detected"); cause details appear below it and in JSON output.

### Two detection sources, one alert model

braid owns btrfs device stats + missing device detection. smartd owns SMART monitoring and writes a flag file (`/var/lib/braid/smartd-alert`) when triggered. The shared computation checks btrfs stats, missing devices, and smartd.

### All five btrfs device stat counters trigger alerts

write_io_errs, read_io_errs, flush_io_errs, corruption_errs, generation_errs. Any non-zero counter above the acked baseline triggers an alert.

### Latched alerts

Alerts persist until `braid ack` — even if the triggering condition disappears. This means "something happened that needs acknowledgment," not "something is currently true." This avoids cross-source bugs where one source clearing could hide another source's alert, and matches Synology UX.

### Ack snapshots gating inputs before probing

`cmd_ack` reads the alert latch, the smartd flag (`smartd-alert`), and the ack cleanup-pending sentinel (`alert-cleanup-pending`) once at function entry, before `probe_pool_alerts`. Every decision in that ack -- the gate that decides whether to proceed, the cleanup-only retry branch, and the cleanup that removes alert files -- references that single snapshot. If the sentinel is the only live signal, `cmd_ack` runs a cleanup-only retry branch before `probe_pool_alerts` so recovery does not depend on probe success and does not rewrite the acknowledged baseline. The alert probe is devid-keyed and intentionally does not depend on LUKS UUID identity or pool FSID. The pool lock at `/run/braid-pool.lock` already serializes monitor vs ack vs add/remove writers, but the smartd hook is intentionally unlocked, so a per-ack snapshot is the only mechanism that gives ack a coherent view of smartd state.

The smartd flag is cleared during cleanup when either the snapshot observed the flag active or the snapshot's latch carried a `SmartdAlert` cause. The first arm covers the normal "flag present, ack silences it" case. The second arm is an explicit exception for the crash-recovery case where a prior cycle latched `SmartdAlert` but the flag was already absent at snapshot, such as a partially-applied earlier ack, manual state, or filesystem-level divergence. The user's ack is aimed at the latched smartd source, so a flag that the smartd hook writes during the probe is part of that source and is cleared.

A flag that exists at cleanup time when the snapshot saw neither active smartd state nor a latched `SmartdAlert` cause arrived after the snapshot and is left in place: the next monitor cycle is responsible for latching it cleanly.

### Ack state keyed by btrfs devid

Acked baselines are keyed by btrfs devid (`acked-stats.json` maps stringified devid to baseline) -- no path or LUKS UUID mapping is required to associate a stats row with its baseline. The parser captures missing device devids from MISSING sentinel lines.

Membership cross-reference is performed at the alert-pipeline boundary, not at the baseline-keying level. `AlertPoolState::recognized_devids` (in `cli/src/probe.rs`) returns the union of `present_devids`, `null_underlying`, and `missing_devids` for the current cycle. Both `compute_alert_state` and `snapshot_current` filter `btrfs device stats` rows against that set before emitting causes or writing baselines. A stats row whose devid is outside the recognized set is treated as transient/stale identity: it cannot latch `BtrfsDeviceErrors`, and `braid ack` does not persist a baseline for it, which prevents a loop on the next monitor cycle's `reconcile_acked_stats` prune.

### Ack state separate from pool.json

Different concerns (identity vs acknowledgment), different write patterns, different risk profiles (precious vs disposable). Stored at `/var/lib/braid/acked-stats.json`.

### Ack state is machine-local

On a new machine, acked state doesn't exist — everything evaluates fresh.

### `braid monitor` is a pure detector

Checks state and returns an exit code. Does not start/stop services. The systemd wrapper starts the beeper on exit 1.

Exit codes:
- **0** -- ok or pool offline with no active alerts
- **1** -- alert active (disk health issue OR indeterminate state latched as `ComputationError` -- e.g. probe failure, parse failure, unmapped device)
- **2** -- pre-`cmd_monitor` setup failure (e.g. pool-lock I/O, config load failure). Reserved for "could not even attempt to detect"; never emitted by `cmd_monitor` itself.

Fail closed: any failure inside `cmd_monitor` that leaves pool state indeterminate latches a `ComputationError` cause and reports exit 1, so the systemd wrapper starts the beeper. Exit 2 means the monitor never ran -- a beep would be meaningless because there is no `AlertState` to report.

Alert-state mutators are serialized by `/run/braid-pool.lock`. Every command that writes `acked-stats.json` or `alert-latch.json` (`monitor`, `ack`, `add`, `remove`, `remove-missing`) acquires `/run/braid-pool.lock` in Rust dispatch (see [ADR 026](026-pool-lock-rust-owned.md)) before reading state or running probes. This is intentionally the same lock used by pool mutators: monitor and ack perform read-modify-write cycles around subprocess I/O, while add/remove/remove-missing prune acked baselines as membership changes. Sharing one lock keeps "baseline and latch clear" authoritative and prevents stale monitor snapshots from resurrecting acknowledged alerts.

Mount presence is read from `/proc/self/mountinfo` via `mount_check::fstype_at_mount_via_fs`, not from `findmnt`. A readable, well-formed mountinfo file with no entry for the configured mount point is legitimate `PoolOffline` and exits 0. Any mountinfo I/O failure, malformed line, or duplicate target entry is indeterminate state: it surfaces as `ProbeError::MountInfo`, latches `ComputationError`, exits 1, and starts the beeper.

Self-heals stale ack state (resets `missing_acked` for now-present devids after drive replacement).

### Periodic one-shot, not daemon

systemd timer + oneshot service. No mount condition on the timer — `braid monitor` handles pool-not-mounted gracefully (exit 0).

### On by default

`braid.monitor.enable` defaults to true when `braid.enable` is true. beep/pcspkr failures are silently swallowed.

### Audible doctor beep is opt-in

Plain `braid doctor` reports the alert-beep check as skipped after confirming
beep monitoring is configured. `braid doctor --beep` runs the canonical
`braid-beep-probe` wrapper so operators can test the real alert sound on
purpose. `braid doctor --json` always skips the audible probe, even when
combined with `--beep`, so machine-readable output has no audible side
effects.

### Latch as append/refresh log

The alert latch is an append/refresh log of all unacked causes from all sources. Each monitor cycle loads the existing latch, computes new causes, and merges. Previously-latched causes that aren't re-detected are carried forward. Newly-detected causes replace their latched counterpart (same key = fresher evidence). This ensures all cause types persist until `braid ack`, even if the triggering condition resolves — fixing the invariant for all sources, not just journal.

### Corrupt latch recovery

`load_alert_latch` returns `Result<Option<AlertState>, LatchLoadError>` so callers can distinguish three outcomes: file absent (`Ok(None)`, normal -- no active alerts), I/O failure (`Err(Read)`), and unparseable on-disk content (`Err(Parse)`). Each caller picks its own fail-closed policy:

- `cmd_monitor` is the only path that mutates the latch. On read/parse failure it quarantines the bad bytes by linking `alert-latch.json` to `alert-latch.json.corrupt` and then removing the live path, then writes a fresh latch containing a loud `ComputationError` cause whose `detail` names the failure. Quarantine uses `hard_link` + `remove_file` (not `rename`) so an already-existing sidecar is detected atomically by `link(2)`'s `EEXIST`; when that happens, the first sidecar is preserved as the highest-value forensic snapshot and the new corruption is surfaced only in the `ComputationError` detail. Any I/O failure during quarantine is folded into the same detail rather than silently dropped. The corruption signal is folded into a single `ComputationError` (not appended as a second cause), because `merge_into_latch` collapses every `ComputationError` into one slot via `same_cause_key` -- appending two would silently drop one.
- `cmd_status` is the read-only surface: `resolve_alert_state` surfaces a corrupt latch as a `ComputationError` cause but never moves the file (status must not mutate state).
- `cmd_ack` treats a corrupt latch as an active alert for gating purposes — otherwise a genuinely unmounted ack would refuse with `PoolNotMounted` and the user would have no way to clear a corrupt file with the pool offline. Mounted ack and genuinely unmounted ack clean up both `alert-latch.json` and the `.corrupt` sidecar. A foreign fstype at the configured mount point is a probe error, not offline ack, and preserves the unreadable latch bytes.

This preserves "latched until ack" even when the on-disk state is unreadable: the operator sees a loud `ComputationError`, the bad bytes are preserved for forensics until an ack path that can safely clean them up, and ack succeeds for mounted or genuinely unmounted pools.

#### Cleanup ordering and retry-on-failure

Ack cleanup preserves three invariants. First, the beeper stop hook is attempted before any fallible cleanup operation; the hook is best-effort, so the invariant is that the stop attempt runs, not that sound was proven stopped. Second, destructive removals run in `smartd-alert` -> `alert-latch.json` -> `alert-latch.json.corrupt` order, so the corrupt sidecar leaves last and the forensic guarantee above is preserved across cleanup failures. Third, ack writes `alert-cleanup-pending` after the stop hook and before the first destructive step, then clears it only after the last destructive step succeeds.

`CleanupFailed` recovery has two cases. If creating `alert-cleanup-pending` itself fails, no destructive removal has run, so the original entry signals are byte-identical and the retry is driven by the normal ack path. If marker creation succeeded and a later step failed, the sentinel remains on disk. `cmd_ack` consults that sentinel before probing; when it is the only live signal, the hoisted cleanup-only branch reruns cleanup without `probe_pool_alerts`, without runner requests, and without rewriting `acked-stats.json`. Either path makes re-running `braid ack` after fixing the I/O fault genuinely idempotent.

### Offline ack policy

`braid ack` works with the pool locked, but only when the pool is genuinely unmounted: `/proc/self/mountinfo` has no entry for the configured mount point. If the configured mount point is occupied by a non-btrfs filesystem, `cmd_ack` returns `ProbeError::NotBtrfs`; it must not clear `alert-latch.json`, remove `smartd-alert`, create or rewrite `acked-stats.json`, stop the beeper, or quarantine corrupt latch bytes.

For genuine offline ack, the persistence layer has an asymmetry by cause type:

- `MissingDevice { devid }` -- offline ack reads the latch and applies `missing_acked = true` to that devid in `acked-stats.json` (insert-or-update; existing `device_stats` baselines are preserved). The next mounted monitor cycle suppresses the cause, and `reconcile_acked_stats` self-heals `missing_acked` back to `false` if the device returns.
- `BtrfsDeviceErrors { devid }` -- offline ack refuses with an actionable error ("cannot ack btrfs device errors while pool is offline -- unlock the pool first"). The counter baseline that suppresses re-firing is the *current* output of `btrfs device stats`, which requires a mounted pool. Refusing the *whole* ack (not partial-acking other causes) avoids leaving the operator in an "I acked but it still says ALERT" state.
- `SmartdAlert` -- offline ack removes the smartd flag file (the authoritative trigger source); no `acked-stats.json` write is needed.
- `ComputationError` -- offline ack removes the latch; the cause re-fires on the next monitor cycle only if the underlying computation still fails.

Coupled to the asymmetry: offline ack only loads `acked-stats.json` when at least one `MissingDevice` cause is latched, so an unrelated corrupt `acked-stats.json` cannot block an offline ack of a pure `SmartdAlert` or `ComputationError` latch. When `acked-stats.json` *is* loaded (a `MissingDevice` cause is being applied), the fail-closed `load_acked_stats_fallible` is used so corrupt files are propagated as I/O errors rather than silently overwritten -- matching the policy in `drop_ghost_acked_for_devids`.

### Acked-stats hygiene across pool membership changes

btrfs allocates new devids as `last_devid + 1` (kernel: `fs/btrfs/volumes.c`, `find_next_devid`), so a `remove`-then-`add` sequence reuses the removed devid only when that devid was the current maximum at remove time. Removing a non-max devid leaves a permanent gap. A stale acked-stats entry for a reused devid would otherwise carry the previous holder's `device_stats` baseline (suppressing health alerts until counters exceed the ghost) or its `missing_acked = true` flag (suppressing missing-device alerts) onto the fresh disk.

Invariant: a reused devid must never inherit the previous holder's ack baseline.

Three layers enforce it:

1. **Add-time guard (correctness boundary):** `cmd_add` clears acked-stats unconditionally on bootstrap and drops the assigned devid per-disk inside the live-pool add loop. `cmd_recover`, when finishing an interrupted add, mirrors both: bootstrap-recovery calls `remove_acked_stats`, and live-add recovery drops every journaled target's devid (per-arm after a replayed `pool_add_device`, and via a final sweep when the target was already live at recovery entry -- the committed-but-closed crash window). Cleanup failure here is command-fatal in both `cmd_add` and `cmd_recover`: the error names the stage and instructs the user to delete the file before relying on alerts.
2. **Remove-time prune (hygiene):** `cmd_remove` and `cmd_remove_missing` drop the affected devid on success. `cmd_recover` mirrors the prune for committed removes only -- the Remove guard may restore a target whose eviction did not complete, in which case its acked-stats entry is a legitimate baseline that must survive. Cleanup failure here is non-fatal (warning) -- the next `add` for that devid will catch it via layer 1. The `cmd_remove` planner enriches the journaled `pre_membership` with the target's live btrfs devid so recovery can resolve it after a discover-time `pool.json`.
3. **Monitor reconcile (defense-in-depth):** `cmd_monitor` prunes orphan entries (devid no longer in `pool.present_devids`, `pool.null_underlying`, or `pool.missing_devids`) every cycle. This catches crash recovery and manual btrfs operations performed outside braid. It cannot detect ghost data once a devid is reused, so the add-time layer is the boundary for that case. The read itself uses `load_acked_stats_fallible` so a corrupt or unreadable `acked-stats.json` latches `ComputationError` instead of silently re-firing acked causes against an empty baseline, matching offline ack and `drop_ghost_acked_for_devids`. A save failure during reconcile latches the same `ComputationError` so a persistent FS write fault (EROFS, ENOSPC, or EACCES on `acked-stats.json` or its parent) surfaces via exit-1 beep rather than accumulating only in journald.

## Rejected alternatives

- **Daemon-based monitoring**: more complex lifecycle management for no benefit over a timer + oneshot
- **Storing alerts in a database**: unnecessary complexity; file-based flag + JSON is sufficient
- **Per-surface alert logic**: each surface re-checking btrfs stats independently would lead to inconsistencies
- **Counter-based thresholds (e.g., alert after N errors)**: any non-zero counter above baseline is worth investigating; thresholds delay detection
- **Kernel journal scanning**: originally implemented as a supplementary alert source scanning `journalctl -k` for "BTRFS error" messages. Removed because btrfs commits every 30 seconds, which increments device stats counters for any disk error within that window. The 5-minute monitor poll catches those counters reliably. Journal scanning was redundant with device stats and added significant complexity (cursor tracking, regex parsing, crash-safe cursor ordering, latch merge logic). Repro VMs in `tests/repro/kernel-journal-*` preserve the empirical evidence from the original investigation.
