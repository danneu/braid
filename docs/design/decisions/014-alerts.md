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

### Shared alert computation, three layers

Alert logic is computed once and flows through three types, mirroring braid's split between internal state and the public DTO. No surface re-encodes alert logic.

- **Live detection** — `compute_alert_state` returns `Vec<AlertCause>`: the untimestamped "what" observed this cycle. A live cause carries no time; the timestamp is a property of being *latched*, not of the cause.
- **Persisted latch** — `AlertState { causes: Vec<LatchedCause> }`, written by `braid monitor` and read by `status`/`ack`. Each `LatchedCause` wraps an `AlertCause` with a required `detected_at` (RFC3339 UTC seconds) recording when the monitor *first* latched that cause. See [First-detected timestamp](#first-detected-timestamp).
- **Status view** — `braid status` (banner + causes, JSON) and the TUI consume `AlertCauseReport { cause, first_detected: Option<String> }`. Latch-derived causes carry `Some(detected_at)`; status-synthesized bridge causes (see [Corrupt latch recovery](#corrupt-latch-recovery)) carry `None`.

`AlertState::severity()` (the monitor exit code and the status/TUI banner) is the max over `c.cause.severity()`.

### First-detected timestamp

Each `LatchedCause` records `detected_at`: an RFC3339-UTC-seconds string stamped when the monitor *first* appended that cause to the latch, and **preserved across refreshes** — a re-detected cause (same `same_cause_key`) keeps its original `detected_at` while taking the fresher cause evidence; only a brand-new append stamps `now`. `braid status` renders it per cause as an absolute timestamp plus a relative age (`-- first detected 2026-06-25T15:35:54Z (2 hours ago)`), making explicit that a latched alert is an *incident* ("this happened, ack it"), not necessarily a live fact.

The timestamp **enriches** the latch; it does not change when alerts appear or clear. The sticky-latch invariant is unchanged. `detected_at` is display-only: the merge path only clones it forward, and only the `status` renderer ever parses it back (to compute the age, degrading to absolute-only on a malformed or future value). RFC3339-UTC-seconds is stored verbatim — no `u64` conversion layer — because nothing integer-compares it (unlike `EnospcAck::snoozed_until`, which is a `u64` precisely because the monitor compares it); it still sorts chronologically as a plain string. `detected_at` is **required** on disk: there are no pre-timestamp latches in the wild, so a cause object without one is malformed and fails to parse, matching the fail-loud latch philosophy. Status-synthesized bridge causes legitimately carry no timestamp and render with none.

### Alert causes

`AlertCause` is an explicit enum:
- `BtrfsDeviceErrors { devid }` — non-zero btrfs device stat counters above acked baseline, excluding alert-local missing devids
- `MissingDevice { devid }` — device missing from pool
- `SmartdAlert` — smartd SMART health warning
- `ScrubFailed` — the scheduled maintenance scrub (`braid-scrub.service`) failed to run or complete (btrfs internal error, a transient device error that aborted the scrub, ENOSPC on metadata, or a spawn failure). Distinct from scrub-*found* corruption, which still alerts via `BtrfsDeviceErrors` through the device-stats poll (see [Two detection sources](#two-detection-sources-one-alert-model)). Flag-driven like `SmartdAlert`; serializes as `{"type":"scrub_failed"}`
- `ComputationError { detail }` — probe or parse failed before a structured cause could be determined
- `EnospcRisk { margin, count_below, device_count }` — pool is one disk-loss away from RAID1 chunk-pair ENOSPC (cannot allocate the chunk pairs to restore redundancy). `margin` is the signed risk magnitude (negative = at-risk depth); the cause deliberately carries no pool identity, so the public `status --json` cause stays a clean risk descriptor and keying lives in `enospc-ack.json` (see [Severity tiers and the ENOSPC baseline](#severity-tiers-and-the-enospc-baseline))

Critical causes render the cause-neutral `CRITICAL alert` banner ("CRITICAL alert -- pool health issue detected. Run 'braid ack' to acknowledge and silence."), while a Warning-only `EnospcRisk` state renders the lower-urgency `WARNING alert` capacity banner ("WARNING alert -- capacity risk detected. Run 'braid ack' to acknowledge."); cause details still appear below the banner and in JSON output. See [Severity tiers and the ENOSPC baseline](#severity-tiers-and-the-enospc-baseline).

### Two detection sources, one alert model

braid owns btrfs device stats + missing device detection. smartd owns SMART monitoring and writes a flag file (`/var/lib/braid/smartd-alert`) when triggered. A third event source is the scheduled scrub: `braid-scrub.service`'s `onFailure` runs `braid-scrub-failed.service`, which writes `/var/lib/braid/scrub-failed`. The shared computation checks btrfs stats, missing devices, the smartd flag, and the scrub-failed flag.

This `ScrubFailed` source covers scrub *execution* failure only. Scrub-*found* corruption is deliberately **not** raised here: an uncorrectable-error scrub completes (btrfs exit 3, declared a service success via `SuccessExitStatus=3`, so `onFailure` never fires), and the corruption it logged into the per-device counters already reaches the operator via the `BtrfsDeviceErrors` device-stats poll. A separate scrub-status probe would be redundant with that pipeline — that long-standing note applies to corruption, not to execution failure.

The whole `ScrubFailed` pipeline is gated on `braid.monitor.enable`: the `onFailure` reference, `braid-scrub-failed.service`, the latch, and `braid ack` all live behind the monitor. Running `autoScrub` without the monitor is unusual but legitimate (an operator who does their own monitoring), so it is a build-time **warning**, not an assertion: braid emits a `warnings` entry for the `autoScrub.enable && !monitor.enable` combination noting that neither a failed scrub nor scrub-discovered corruption will raise any alert.

### All five btrfs device stat counters trigger alerts

write_io_errs, read_io_errs, flush_io_errs, corruption_errs, generation_errs. Any non-zero counter above the acked baseline triggers an alert for a present recognized devid. Devids in the alert-local missing set are excluded from `BtrfsDeviceErrors` and alert through `MissingDevice` instead.

Two kernel paths feed those counters: ordinary I/O and scrub. Scrub records
read, checksum, and generation failures by incrementing
`BTRFS_DEV_STAT_READ_ERRS`, `BTRFS_DEV_STAT_CORRUPTION_ERRS`, and
`BTRFS_DEV_STAT_GENERATION_ERRS` in
`reference/linux/fs/btrfs/scrub.c:985-993`. The monitor polls the same device
stats either way, so scrub-discovered uncorrectable errors reach the operator
through the same `BtrfsDeviceErrors` cause and beep as everyday I/O errors; a
separate scrub-status alert probe would be redundant with this pipeline.

### Latched alerts

Alerts persist until `braid ack` — even if the triggering condition disappears. This means "something happened that needs acknowledgment," not "something is currently true." This avoids cross-source bugs where one source clearing could hide another source's alert, and matches Synology UX.

### Ack snapshots gating inputs before probing

`cmd_ack` reads the alert latch, the smartd flag (`smartd-alert`), the scrub-failed flag (`scrub-failed`), and the ack cleanup-pending sentinel (`alert-cleanup-pending`) once at function entry, before `probe_pool_alerts`. Every decision in that ack -- the gate that decides whether to proceed, the cleanup-only retry branch, and the cleanup that removes alert files -- references that single snapshot. If the sentinel is the only live signal, `cmd_ack` runs a cleanup-only retry branch before `probe_pool_alerts` so recovery does not depend on probe success and does not rewrite the acknowledged baseline. The alert probe is devid-keyed and intentionally does not depend on LUKS UUID identity or pool FSID. The pool lock at `/run/braid-pool.lock` already serializes monitor vs ack vs add/remove writers, but the smartd and scrub-failed hooks are intentionally unlocked, so a per-ack snapshot is the only mechanism that gives ack a coherent view of that flag state.

The smartd flag is cleared during cleanup when either the snapshot observed the flag active or the snapshot's latch carried a `SmartdAlert` cause. The first arm covers the normal "flag present, ack silences it" case. The second arm is an explicit exception for the crash-recovery case where a prior cycle latched `SmartdAlert` but the flag was already absent at snapshot, such as a partially-applied earlier ack, manual state, or filesystem-level divergence. The user's ack is aimed at the latched smartd source, so a flag that the smartd hook writes during the probe is part of that source and is cleared.

A flag that exists at cleanup time when the snapshot saw neither active smartd state nor a latched `SmartdAlert` cause arrived after the snapshot and is left in place: the next monitor cycle is responsible for latching it cleanly.

The scrub-failed flag follows the identical two-arm rule against `ScrubFailed`: cleared when the snapshot saw the flag active or the latch carried `ScrubFailed`, and otherwise preserved for the next monitor cycle.

### Ack state keyed by btrfs devid

Acked baselines are keyed by btrfs devid (`acked-stats.json` maps stringified devid to baseline) -- no path or LUKS UUID mapping is required to associate a stats row with its baseline. The parser captures missing device devids from MISSING sentinel lines.

Membership cross-reference is performed at the alert-pipeline boundary, not at the baseline-keying level. `AlertPoolState::alert_devids` (in `cli/src/probe.rs`) builds the `AlertDevids` carrier whose `recognized` field is the union of `present_devids`, `null_underlying`, and `missing_devids` for the current cycle (the carrier groups it with the alert-local `missing` set so the two can never be swapped positionally). Both `compute_alert_state` and `snapshot_current` take the carrier and filter `btrfs device stats` rows against the recognized set before emitting causes or writing baselines. A stats row whose devid is outside the recognized set is treated as transient/stale identity: it cannot latch `BtrfsDeviceErrors`, and `braid ack` does not persist a baseline for it, which prevents a loop on the next monitor cycle's `reconcile_acked_stats` prune.

Within the recognized set, `compute_alert_state` also skips rows whose devid is in the alert-local missing set (`missing_devids` plus `null_underlying`). Those rows alert through `MissingDevice`, not `BtrfsDeviceErrors`, regardless of the device string btrfs printed. `snapshot_current` still records recognized rows by devid before layering `missing_acked = true` from the missing set, so a returning member does not re-alert on stale counters already acknowledged while missing.

### Ack state separate from pool.json

Different concerns (identity vs acknowledgment), different write patterns, different risk profiles (precious vs disposable). Stored at `/var/lib/braid/acked-stats.json`.

### Ack state is machine-local

On a new machine, acked state doesn't exist — everything evaluates fresh.

### `braid monitor` is a pure detector

Checks state and returns an exit code. Does not start/stop services. The systemd wrapper maps each exit code to a notification unit.

The exit-code → wrapper-action numbers are owned by [ADR 018's canonical exit-code table](018-systemd-lifecycle.md#braid-monitortimer--braid-monitorservice--health-polling), so the two Active ADRs cannot drift. This section owns the severity → beep *semantics* that decide which number `cmd_monitor` returns:

- The audible beep is reserved for **Critical** causes. `BtrfsDeviceErrors`, `MissingDevice`, `SmartdAlert`, `ScrubFailed`, and `ComputationError` are Critical (`ComputationError` is fail-closed/indeterminate, so it must beep; a failed scrub beeps exactly like the smartd source it mirrors). A Critical alert reports exit 1.
- `EnospcRisk` is the only **Warning** cause: it notifies via `alertCommand` + `braid status` but does not beep, so the operator is not trained to mute the channel built for a dying disk. A Warning-only cycle reports exit 3.
- A mixed Warning+Critical cycle reports the Critical exit (1): `AlertState::severity()` returns the max over causes.

Fail closed: any failure inside `cmd_monitor` that leaves pool state indeterminate latches a `ComputationError` cause and reports exit 1, so the systemd wrapper starts the beeper. **One exception** -- the best-effort `btrfs device usage` ENOSPC probe: a probe, parse, or marker-load failure there skips *only* the `EnospcRisk` cause and deliberately does **not** latch `ComputationError`, so a broken usage probe never masks device-error / missing-device alerting in the same cycle. This is the sole carve-out to the fail-closed mandate; the probe mechanism is documented in [ADR 018](018-systemd-lifecycle.md#braid-monitortimer--braid-monitorservice--health-polling).

### Severity tiers and the ENOSPC baseline

`AlertSeverity` has two tiers, `Warning < Critical`. Every cause has a severity (see the cause list above), and `AlertState::severity()` is the max over its causes. The split exists so a proactive, non-beeping capacity warning is not delivered through the audible channel built for a dying disk: the beep is reserved for Critical, `braid status` and the TUI render `CRITICAL alert -- ...` for Critical and a distinct lower-urgency `WARNING alert -- ...` banner for a Warning-only alert, and the monitor routes a Warning-only cycle to exit 3.

`EnospcRisk` uses a **time-based snooze/reminder** model for ack/re-alert, stored in a dedicated `enospc-ack.json`, not `acked-stats.json` (the two key on different things — error counters vs pool geometry). ("baseline" survives here only as this heading's stable anchor and `enospc-ack.json`'s historical nickname; the margin-baseline model it once named is gone.) Ack = **snooze**, not resolve: it silences the *reminder* for an interval, and `braid status` keeps showing the live ENOSPC advisory the whole time (`build_status` recomputes risk independently of the marker).

- **Ack snoozes the reminder.** A mounted `braid ack` of an `EnospcRisk` latch re-probes `btrfs device usage`; if the pool is **still at risk**, and the fresh usage snapshot contains **no zero-sized device**, it writes a marker `{ pool_key, snoozed_until }` whose deadline is one `ENOSPC_REMINDER_INTERVAL` (7 days) past ack time. If the pool has already recovered by ack time, or the fresh usage snapshot contains **any device with `device_size == 0`** -- whether a btrfs missing member (rendered `<missing disk>`) or a present device whose size probe failed (real path, size 0) -- it writes **no** marker; a later recurrence then alerts immediately.
- **Remind after the interval.** The monitor suppresses `EnospcRisk` only while `now` is inside the snooze window; once the deadline elapses it re-fires **every cycle** (sticky latch, the same cadence as the first alert) until the operator re-acks, which stamps a fresh deadline one interval out. A deadline more than one interval beyond `now` is treated as elapsed — the clock moved between ack and now (ahead at ack, or corrected backward since) — which bounds any clock anomaly to a single interval and fails toward reminding.
- **Re-arm on clear.** When the predicate's own surplus recovers past the re-arm margin, the monitor drops the marker so a future recurrence alerts fresh. Re-arm keys off the *predicate margin*, not raw min-headroom, so a fault-tolerant pool with one low device still re-arms.
- **Written only by `braid ack`, removed only by the monitor** (re-arm, confirmed key mismatch, corruption, or a loaded key carrying a zero-sized missing-device entry). The marker persists past ack — it is the post-ack snooze memory — so ack cleanup deliberately does not delete it. A loaded marker whose `pool_key` already contains `device_size == 0` is positively invalid: the monitor fires armed and removes it before any key comparison can suppress.

No on-disk migration ships for the (unreleased) margin-baseline file: a stale `{ pool_key, baseline_margin }` marker is missing `snoozed_until`, so it fails to deserialize and falls through the existing corrupt-marker path (fire armed, then remove).

**Marker identity (`pool_key`).** The snooze marker is bound to a `pool_key`: the btrfs filesystem UUID plus the sorted per-device `(devid, device_size)` pairs, captured at ack time. The monitor discards a marker whose key differs from the live pool, so a marker acked on an old pool (bootstrap/recreate → new FS UUID), an old membership (add/remove → changed devid set), **or** an old geometry (`braid replace`/resize → same devid, changed `device_size`) cannot suppress a fresh `EnospcRisk`. Keying on `device_size`, not just devid, is what closes the same-devid replace gap: `btrfs replace` keeps the source devid but changes the chunk-pair capacity geometry the predicate depends on, so `fsid + devids` alone would stay identical. `device_size` is stable across ordinary fill (only `used`/`unallocated` move), so the key does not churn while a pool merely fills — it changes only on a real topology/geometry event. This is the `EnospcRisk` analog of the membership-change hygiene `acked-stats.json` gets from [reconcile plus the add/remove/recover ghost-drop callers](#acked-stats-hygiene-across-pool-membership-changes); keying the marker is self-validating, so a missed command hook cannot reintroduce the stale-marker class.

One accepted race remains: `missing_count` comes from `btrfs filesystem show`, while the live `pool_key` comes from a later `btrfs device usage --raw` probe. If a device drops between those probes, show can still report no missing devices while usage already renders the missing member as `(devid, 0)`. Against a clean stored marker this is safe: the live key differs from the stored `(devid, device_size)` key, so the monitor treats it as a confirmed key mismatch, fires armed, and removes the marker; the next cycle either sees `MissingDevice` through show or re-probes a clean pool. The invariant is that a zero-sized device never appears in a baseline that suppresses an alert: ack never writes such a key, and the monitor never honors one already on disk.

When the live key cannot be built (the probe yields no FS UUID), the monitor treats it as an identity *gap*, not a confirmed different pool: it fires armed if at risk but leaves any stored marker in place, so a later cycle with the FS UUID present can compare and re-arm it.

This differs from the "latched until ack even if the condition disappears" rule only in the *post-ack* marker (re-arm on clear), exactly as `MissingDevice` + `missing_acked` already self-re-arm via `reconcile_acked_stats`. The latch itself stays sticky-until-ack (`merge_into_latch` carries it forward), so the invariant holds.

### `braid ack` acknowledges current alerts

Exit codes:
- **0** -- current alerts acknowledged, or no active alerts needed acknowledgment
- **1** -- pool-lock contention, or `cmd_ack` attempted acknowledgment and failed (for example offline btrfs-error refusal, probe/fstype error, or cleanup I/O)
- **2** -- pre-`cmd_ack` setup failure (config load failure or pool-lock I/O). Reserved for "could not even attempt to acknowledge"; never emitted by `cmd_ack` itself.

Ack uses the same setup-failure convention as monitor but treats contention differently: ack is interactive, so a missed acknowledgment run reports failure with exit 1 instead of monitor's harmless exit-0 timer skip.

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
purpose. `braid doctor --json` always skips the audible probe, and `--json`
conflicts with `--beep` at parse time, so machine-readable output has no
audible side effects.

### Latch as append/refresh log

The alert latch is an append/refresh log of `LatchedCause` entries (each an `AlertCause` plus its `detected_at`) covering all unacked causes from all sources. Each monitor cycle loads the existing latch, computes new live causes (`Vec<AlertCause>`), and merges. Previously-latched entries that aren't re-detected are carried forward verbatim. A newly-detected cause that matches a latched entry by key refreshes that entry's cause (fresher evidence) while **keeping its original `detected_at`** (see [First-detected timestamp](#first-detected-timestamp)); a cause with no latched counterpart is appended as a fresh entry stamped with the current time. This ensures all cause types persist until `braid ack`, even if the triggering condition resolves — fixing the invariant for all sources, not just journal.

### Corrupt latch recovery

`load_alert_latch` returns `Result<Option<AlertState>, LatchLoadError>` so callers can distinguish three outcomes: file absent (`Ok(None)`, normal -- no active alerts), I/O failure (`Err(Read)`), and unparseable on-disk content (`Err(Parse)`). A latch whose cause object is missing the required `detected_at` is unparseable and falls into `Err(Parse)` like any other malformed content -- there are no pre-timestamp latches to accept (see [First-detected timestamp](#first-detected-timestamp)). Each caller picks its own fail-closed policy:

- `cmd_monitor` is the only path that mutates the latch. On read/parse failure it quarantines the bad bytes by linking `alert-latch.json` to `alert-latch.json.corrupt` and then removing the live path, then writes a fresh latch containing a loud `ComputationError` cause whose `detail` names the failure. Quarantine uses `hard_link` + `remove_file` (not `rename`) so an already-existing sidecar is detected atomically by `link(2)`'s `EEXIST`; when that happens, the first sidecar is preserved as the highest-value forensic snapshot and the new corruption is surfaced only in the `ComputationError` detail. Any I/O failure during quarantine is folded into the same detail rather than silently dropped. The corruption signal is folded into a single `ComputationError` (not appended as a second cause), because `merge_into_latch` collapses every `ComputationError` into one slot via `same_cause_key` -- appending two would silently drop one.
- `cmd_status` is the read-only surface: `resolve_alert_state` surfaces a corrupt latch as a `ComputationError` cause but never moves the file (status must not mutate state).
- `cmd_ack` treats a corrupt latch as an active alert for gating purposes — otherwise a genuinely unmounted ack would refuse with `PoolNotMounted` and the user would have no way to clear a corrupt file with the pool offline. Mounted ack and genuinely unmounted ack clean up both `alert-latch.json` and the `.corrupt` sidecar. A foreign fstype at the configured mount point is a probe error, not offline ack, and preserves the unreadable latch bytes.

This preserves "latched until ack" even when the on-disk state is unreadable: the operator sees a loud `ComputationError`, the bad bytes are preserved for forensics until an ack path that can safely clean them up, and ack succeeds for mounted or genuinely unmounted pools.

#### Cleanup ordering and retry-on-failure

Ack cleanup preserves three invariants. First, the beeper stop hook is attempted before any fallible cleanup operation; the hook is best-effort, so the invariant is that the stop attempt runs, not that sound was proven stopped. Second, destructive removals run in `smartd-alert` -> `scrub-failed` -> `alert-latch.json` -> `alert-latch.json.corrupt` order, so the corrupt sidecar leaves last and the forensic guarantee above is preserved across cleanup failures. Third, ack writes `alert-cleanup-pending` after the stop hook and before the first destructive step, then clears it only after the last destructive step succeeds.

`CleanupFailed` recovery has two cases. If creating `alert-cleanup-pending` itself fails, no destructive removal has run, so the original entry signals are byte-identical and the retry is driven by the normal ack path. If marker creation succeeded and a later step failed, the sentinel remains on disk. `cmd_ack` consults that sentinel before probing; when it is the only live signal, the hoisted cleanup-only branch reruns cleanup without `probe_pool_alerts`, without runner requests, and without rewriting `acked-stats.json`. Either path makes re-running `braid ack` after fixing the I/O fault genuinely idempotent.

### Offline ack policy

`braid ack` works with the pool locked, but only when the pool is genuinely unmounted: `/proc/self/mountinfo` has no entry for the configured mount point. If the configured mount point is occupied by a non-btrfs filesystem, `cmd_ack` returns `ProbeError::NotBtrfs`; it must not clear `alert-latch.json`, remove `smartd-alert`, create or rewrite `acked-stats.json`, stop the beeper, or quarantine corrupt latch bytes.

For genuine offline ack, the persistence layer has an asymmetry by cause type:

- `MissingDevice { devid }` -- offline ack reads the latch and applies `missing_acked = true` to that devid in `acked-stats.json` (insert-or-update; existing `device_stats` baselines are preserved). The next mounted monitor cycle suppresses the cause, and `reconcile_acked_stats` self-heals `missing_acked` back to `false` if the device returns.
- `BtrfsDeviceErrors { devid }` -- offline ack refuses with an actionable error ("cannot ack btrfs device errors while pool is offline -- unlock the pool first"). The counter baseline that suppresses re-firing is the *current* output of `btrfs device stats`, which requires a mounted pool. Refusing the *whole* ack (not partial-acking other causes) avoids leaving the operator in an "I acked but it still shows an alert" state.
- `SmartdAlert` -- offline ack removes the smartd flag file (the authoritative trigger source); no `acked-stats.json` write is needed.
- `ScrubFailed` -- offline ack removes the scrub-failed flag file (mirroring `SmartdAlert`); no `acked-stats.json` write is needed. It falls through the `BtrfsDeviceErrors` offline refusal and the `MissingDevice` filter unchanged.
- `ComputationError` -- offline ack removes the latch; the cause re-fires on the next monitor cycle only if the underlying computation still fails.
- `EnospcRisk` -- offline ack is allowed (it carries no monotonic counter, unlike `BtrfsDeviceErrors`) and clears the latch, but writes **no** marker: offline ack cannot probe the live `pool_key`, and a keyless marker would be invalidated anyway. If the pool remounts still at-risk, the monitor re-fires `EnospcRisk` at the quiet Warning level (exit 3, no beep) on the next and each subsequent mounted cycle until a mounted ack snoozes it (writes the reminder deadline). Acceptable for a non-beeping advisory; avoids an offline dependency on `pool.json` membership.

Coupled to the asymmetry: offline ack only loads `acked-stats.json` when at least one `MissingDevice` cause is latched, so an unrelated corrupt `acked-stats.json` cannot block an offline ack of a pure `SmartdAlert` or `ComputationError` latch. When `acked-stats.json` *is* loaded (a `MissingDevice` cause is being applied), the fail-closed `load_acked_stats_fallible` is used so corrupt files are propagated as I/O errors rather than silently overwritten -- matching the policy in `drop_ghost_acked_for_devids`.

### Acked-stats hygiene across pool membership changes

btrfs allocates new devids as `last_devid + 1` (kernel: `fs/btrfs/volumes.c`, `find_next_devid`), so a `remove`-then-`add` sequence reuses the removed devid only when that devid was the current maximum at remove time. Removing a non-max devid leaves a permanent gap. A stale acked-stats entry for a reused devid would otherwise carry the previous holder's `device_stats` baseline (suppressing health alerts until counters exceed the ghost) or its `missing_acked = true` flag (suppressing missing-device alerts) onto the fresh disk.

Invariant: a reused devid must never inherit the previous holder's ack baseline.

Three layers enforce it:

1. **Add-time guard (correctness boundary):** `cmd_add` clears acked-stats unconditionally on bootstrap and drops the assigned devid per-disk inside the live-pool add loop. `cmd_recover`, when finishing an interrupted add, mirrors both: bootstrap-recovery calls `remove_acked_stats`, and live-add recovery drops every journaled target's devid (per-arm after a replayed `pool_add_device`, and via a final sweep when the target was already live at recovery entry -- the committed-but-closed crash window). Cleanup failure here is command-fatal in both `cmd_add` and `cmd_recover`: the error names the stage and instructs the user to delete the file before relying on alerts.
2. **Remove-time prune (hygiene):** `cmd_remove` and `cmd_remove_missing` drop the affected devid on success. `cmd_recover` mirrors the prune for committed removes only -- the Remove guard may restore a target whose eviction did not complete, in which case its acked-stats entry is a legitimate baseline that must survive. Cleanup failure here is non-fatal (warning) -- the next `add` for that devid will catch it via layer 1. The `cmd_remove` planner enriches the journaled `pre_membership` with the target's live btrfs devid so recovery can resolve it after a discover-time `pool.json`.
3. **Monitor reconcile (defense-in-depth):** `cmd_monitor` prunes orphan entries (devid no longer in `pool.present_devids`, `pool.null_underlying`, or `pool.missing_devids`) every cycle. This catches crash recovery and manual btrfs operations performed outside braid. It cannot detect ghost data once a devid is reused, so the add-time layer is the boundary for that case. The read itself uses `load_acked_stats_fallible` so a corrupt or unreadable `acked-stats.json` latches `ComputationError` instead of silently re-firing acked causes against an empty baseline, matching offline ack and `drop_ghost_acked_for_devids`. A save failure during reconcile latches the same `ComputationError` so a persistent FS write fault (EROFS, ENOSPC, or EACCES on `acked-stats.json` or its parent) surfaces via exit-1 beep rather than accumulating only in journald.

Backstop: independently of those three layers, the alert computation fails loud when the acked baseline is no longer comparable to the current counter stream. `compute_alert_state` treats an acked counter that exceeds the current `btrfs device stats` value as 0 and alerts on any nonzero current. btrfs device-stats counters are persistent and monotonic (reset only by `-z`, which braid never runs), so the only ways a current value can sit below the ack baseline are a reused devid that inherited a ghost baseline before add/recover cleanup dropped its acked entry (the committed-but-closed crash window above), or an operator resetting the live counters with `btrfs device stats -z`. The three layers aim to *remove* a stale baseline; this guard ensures that if one transiently survives, it cannot *suppress* a later nonzero counter.

## Rejected alternatives

- **Daemon-based monitoring**: more complex lifecycle management for no benefit over a timer + oneshot
- **Storing alerts in a database**: unnecessary complexity; file-based flag + JSON is sufficient
- **Per-surface alert logic**: each surface re-checking btrfs stats independently would lead to inconsistencies
- **Counter-based thresholds (e.g., alert after N errors)**: any non-zero counter above baseline is worth investigating; thresholds delay detection
- **Kernel journal scanning**: originally implemented as a supplementary alert source scanning `journalctl -k` for "BTRFS error" messages. Removed because btrfs commits every 30 seconds, which increments device stats counters for any disk error within that window. The 5-minute monitor poll catches those counters reliably. Journal scanning was redundant with device stats and added significant complexity (cursor tracking, regex parsing, crash-safe cursor ordering, latch merge logic). Repro VMs in `tests/repro/kernel-journal-*` preserve the empirical evidence from the original investigation.
