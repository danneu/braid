# First-Class Alerts for Disk Health

Status: Active

## Context

Synology NAS boxes beep when a disk develops bad sectors — you hear it, SSH in, and deal with it. Without active alerting, a braid NAS user has no idea anything is wrong unless they happen to run `braid status`.

## Decision

### Alert as primary domain concept

braid has first-class Alerts. An Alert represents "something happened that needs human acknowledgment." Beeping is one notification mechanism for an active alert. `braid status` is the primary surface for understanding alert details. `braid ack` acknowledges current alerts and silences notifications.

### Shared alert computation

A single `compute_alert_state()` function produces an `AlertState` consumed by all surfaces — `braid monitor` (exit code), `braid status` (banner + causes), TUI (banner + indicators). No surface re-encodes alert logic.

### Alert causes

`AlertCause` is an explicit enum:
- `BtrfsDeviceErrors { devid }` — non-zero btrfs device stat counters above acked baseline
- `MissingDevice { devid }` — device missing from pool
- `SmartdAlert` — smartd SMART health warning
- `KernelJournalError { message, cursor, disk_name }` — btrfs error in the kernel journal

The status banner is cause-neutral ("disk health issue detected"); cause details appear below it and in JSON output.

### Three detection sources, one alert model

braid owns btrfs device stats + missing device detection + kernel journal monitoring. smartd owns SMART monitoring and writes a flag file (`/var/lib/braid/smartd-alert`) when triggered. `compute_alert_state()` checks btrfs stats, missing devices, and smartd. Journal causes are merged directly into the latch (see below).

### All five btrfs device stat counters trigger alerts

write_io_errs, read_io_errs, flush_io_errs, corruption_errs, generation_errs. Any non-zero counter above the acked baseline triggers an alert.

### Latched alerts

Alerts persist until `braid ack` — even if the triggering condition disappears. This means "something happened that needs acknowledgment," not "something is currently true." This avoids cross-source bugs where one source clearing could hide another source's alert, and matches Synology UX.

### Ack state keyed by btrfs devid

devid is btrfs-native — no cross-referencing config or disk-map needed. The parser captures missing device devids from MISSING sentinel lines.

### Ack state separate from disk-map.json

Different concerns (identity vs acknowledgment), different write patterns, different risk profiles (precious vs disposable). Stored at `/var/lib/braid/acked-stats.json`.

### Ack state is machine-local

On a new machine, acked state doesn't exist — everything evaluates fresh.

### `braid monitor` is a pure detector

Checks state and returns an exit code. Does not start/stop services. The systemd wrapper starts the beeper on exit 1.

Exit codes:
- **0** — ok or pool offline with no active alerts
- **1** — alert active (disk health issue detected, including pool-offline with latched journal alert)
- **2** — monitor execution error (config, probe, parse, or unmapped device)

Self-heals stale ack state (resets `missing_acked` for now-present devids after drive replacement).

### Periodic one-shot, not daemon

systemd timer + oneshot service. No mount condition on the timer — `braid monitor` handles pool-not-mounted gracefully (exit 0).

### On by default

`braid.monitor.enable` defaults to true when `braid.enable` is true. beep/pcspkr failures are silently swallowed.

### Latch as append/refresh log

The alert latch is an append/refresh log of all unacked causes from all sources. Each monitor cycle loads the existing latch, computes new causes, and merges. Previously-latched causes that aren't re-detected are carried forward. Newly-detected causes replace their latched counterpart (same key = fresher evidence). This ensures all cause types persist until `braid ack`, even if the triggering condition resolves — fixing the invariant for all sources, not just journal.

### Kernel journal monitoring

braid reads the kernel journal (`journalctl -k -o json`) for btrfs error messages as a supplementary alert source. This catches errors that btrfs device stats counters may not reflect immediately, such as transient write failures.

Journal entries are events, not pollable state. The cursor advances past them after each scan. To honor the "latched until ack" invariant, journal-derived causes are merged directly into the alert latch and persist until `braid ack`. The cursor is only advanced after causes are durably merged into the latch.

Journal scanning runs before the pool-mounted check so that boot-time and unlock-time btrfs errors are caught.

Device identity is extracted from journal MESSAGE and structured fields by matching the `braid-<name>` mapper naming convention.

`braid ack` advances the cursor to the present, preventing replay.

Cursor stored at `/var/lib/braid/journal-cursor`. v1 matches only `BTRFS error` in the MESSAGE field. Broader patterns deferred until confirmed by repro tests.

## Rejected alternatives

- **Daemon-based monitoring**: more complex lifecycle management for no benefit over a timer + oneshot
- **Storing alerts in a database**: unnecessary complexity; file-based flag + JSON is sufficient
- **Per-surface alert logic**: each surface re-checking btrfs stats independently would lead to inconsistencies
- **Counter-based thresholds (e.g., alert after N errors)**: any non-zero counter above baseline is worth investigating; thresholds delay detection
