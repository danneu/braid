# First-Class Alerts for Disk Health

Status: Active

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

### Ack state keyed by btrfs devid

devid is btrfs-native — no cross-referencing config or disk-map needed. The parser captures missing device devids from MISSING sentinel lines.

### Ack state separate from pool.json

Different concerns (identity vs acknowledgment), different write patterns, different risk profiles (precious vs disposable). Stored at `/var/lib/braid/acked-stats.json`.

### Ack state is machine-local

On a new machine, acked state doesn't exist — everything evaluates fresh.

### `braid monitor` is a pure detector

Checks state and returns an exit code. Does not start/stop services. The systemd wrapper starts the beeper on exit 1.

Exit codes:
- **0** -- ok or pool offline with no active alerts
- **1** -- alert active (disk health issue OR indeterminate state latched as `ComputationError` -- e.g. probe failure, parse failure, unmapped device)
- **2** -- pre-monitor setup failure (config unreadable). Reserved for "could not even attempt to detect"; never emitted by `cmd_monitor` itself.

Fail closed: any failure inside `cmd_monitor` that leaves pool state indeterminate latches a `ComputationError` cause and reports exit 1, so the systemd wrapper starts the beeper. Exit 2 means the monitor never ran -- a beep would be meaningless because there is no `AlertState` to report.

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

- `cmd_monitor` is the only path that mutates the latch. On read/parse failure it quarantines the bad bytes by renaming `alert-latch.json` to `alert-latch.json.corrupt`, then writes a fresh latch containing a loud `ComputationError` cause whose `detail` names the failure. The corruption signal is folded into a single `ComputationError` (not appended as a second cause), because `merge_into_latch` collapses every `ComputationError` into one slot via `same_cause_key` — appending two would silently drop one.
- `cmd_status` is the read-only surface: `resolve_alert_state` surfaces a corrupt latch as a `ComputationError` cause but never moves the file (status must not mutate state).
- `cmd_ack` treats a corrupt latch as an active alert for gating purposes — otherwise `ack_offline` would refuse with `PoolNotMounted` and the user would have no way to clear a corrupt file with the pool offline. ack always cleans up both `alert-latch.json` and the `.corrupt` sidecar.

This preserves "latched until ack" even when the on-disk state is unreadable: the operator sees a loud `ComputationError`, the bad bytes are preserved for forensics, and ack always succeeds.

### Acked-stats hygiene across pool membership changes

btrfs allocates new devids as `last_devid + 1` (kernel: `fs/btrfs/volumes.c`, `find_next_devid`), so a `remove`-then-`add` sequence reuses the removed devid only when that devid was the current maximum at remove time. Removing a non-max devid leaves a permanent gap. A stale acked-stats entry for a reused devid would otherwise carry the previous holder's `device_stats` baseline (suppressing health alerts until counters exceed the ghost) or its `missing_acked = true` flag (suppressing missing-device alerts) onto the fresh disk.

Invariant: a reused devid must never inherit the previous holder's ack baseline.

Three layers enforce it:

1. **Add-time guard (correctness boundary):** `cmd_add` clears acked-stats unconditionally on bootstrap (every existing entry is stale because the pool's identity is new) and drops the assigned devid's entry per-disk inside the live-pool add loop (so partial multi-add still cleans up the disks that were introduced before a later failure). Cleanup failure here is command-fatal: returning success with a known stale baseline would let the user trust health monitoring on a pool the alert layer cannot reason about. The error names the stage and instructs the user to delete the file before relying on alerts.
2. **Remove-time prune (hygiene):** `cmd_remove` and `cmd_remove_missing` drop the affected devid's acked-stats entry on success. Cleanup failure here is non-fatal (warning) -- the next `add` for that devid will catch it via layer 1.
3. **Monitor reconcile (defense-in-depth):** `cmd_monitor` prunes orphan entries (devid no longer in `pool.devices`, `pool.null_underlying`, or `pool.missing_devids`) every cycle. This catches crash recovery and manual btrfs operations performed outside braid. It cannot detect ghost data once a devid is reused, so the add-time layer is the boundary for that case.

## Rejected alternatives

- **Daemon-based monitoring**: more complex lifecycle management for no benefit over a timer + oneshot
- **Storing alerts in a database**: unnecessary complexity; file-based flag + JSON is sufficient
- **Per-surface alert logic**: each surface re-checking btrfs stats independently would lead to inconsistencies
- **Counter-based thresholds (e.g., alert after N errors)**: any non-zero counter above baseline is worth investigating; thresholds delay detection
- **Kernel journal scanning**: originally implemented as a supplementary alert source scanning `journalctl -k` for "BTRFS error" messages. Removed because btrfs commits every 30 seconds, which increments device stats counters for any disk error within that window. The 5-minute monitor poll catches those counters reliably. Journal scanning was redundant with device stats and added significant complexity (cursor tracking, regex parsing, crash-safe cursor ordering, latch merge logic). Repro VMs in `tests/repro/kernel-journal-*` preserve the empirical evidence from the original investigation.
