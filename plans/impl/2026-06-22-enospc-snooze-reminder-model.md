# Plan: time-based ENOSPC snooze/reminder model

## Context

`braid monitor` raises a non-beeping **Warning** when a RAID1 pool is one disk-loss
from being unable to allocate the chunk pairs needed to restore redundancy
(`EnospcRisk`). Today, once an operator runs `braid ack`, the monitor stores a
**margin baseline** (`EnospcAck { baseline_margin, pool_key }` in
`/var/lib/braid/enospc-ack.json`) and only re-alerts when the live margin falls
more than `ENOSPC_WORSEN_STEP` (512 MiB) below that baseline:

```rust
margin < baseline.baseline_margin.saturating_sub(ENOSPC_WORSEN_STEP as i64)
```

This "materially worse than where you happened to ack" rule is hard to explain and
its behavior depends on the operator's ack-time margin. We are replacing it with a
plain **snooze/reminder** model, like an email reminder: `braid ack` snoozes the
reminder for a fixed interval (**7 days**, chosen for this plan); if the risk still
holds when the interval elapses, the monitor reminds again; ack re-snoozes. Ack
means "I saw this, stop reminding me for now" -- not "resolved." `braid status`
keeps showing the live ENOSPC advisory the whole time.

This is a re-alert *policy* change inside the existing alert architecture (ADR 014).
It does not change severity, exit codes, the beep policy, ackability, the pool-key
identity guard, or the fail-open carve-out.

### Why this is a clean change (verified against the code)

- **`braid status`'s ENOSPC advisory is already independent of the marker.**
  `status.rs` (`build_status`) recomputes risk live via `capacity::enospc_risk_advisory(...)`
  into `report.advisories` -- it never reads `enospc-ack.json` or the latch. So the
  "status keeps showing the advisory after ack" requirement is satisfied with **zero
  status changes**. Do not touch `enospc_risk_advisory` or the status advisory path.
- **`baseline_margin` has no other consumers.** Only the write (`ack::write_enospc_baseline`),
  the compare (`monitor::evaluate_enospc_for_monitor`), and one test helper read it.
- **The project already has the time-injection pattern to mirror.** `membership.rs`
  threads an injectable `now: std::time::SystemTime` (production passes
  `SystemTime::now()`, tests pass `UNIX_EPOCH + Duration`). The `time` crate is a dep.
  We mirror this -- no `Clock` trait, no new abstraction.
- **No on-disk migration needed** (the margin-baseline code is unreleased). A stale
  `baseline_margin`-shaped file fails to deserialize (`snoozed_until` missing) and is
  handled by the existing corrupt-marker path: fire armed + remove. Note this in the
  ADR; write no migration shim. Pinned by a regression test
  (`cmd_monitor_old_margin_shaped_marker_fires_armed_and_clears`) so a future
  `#[serde(default)]` slip cannot silently change the path.

## Desired contract (target behavior)

1. Risk appears -> `monitor` raises Warning, exits 3, advisory service runs
   `alertCommand` once, no beep. (Unchanged.)
2. `braid ack` -> clears the latch; does **not** resolve. If the fresh ack-time usage
   probe confirms the pool is **still at risk**, it also writes a snooze marker
   `{ pool_key, snoozed_until }`; if the risk has already cleared by ack time it writes
   **no** marker (so a later recurrence alerts immediately). `braid status` still shows
   the live advisory.
3. While risk holds and the snooze window is open -> monitor stays quiet (exit 0).
4. Snooze window elapses and risk still holds -> monitor re-fires `EnospcRisk` every
   cycle (sticky latch, identical to the first-alert cadence) until the operator
   re-acks, which writes a fresh deadline one interval out.
5. Risk clears (margin recovers past `ENOSPC_REARM_MARGIN`) -> marker removed; a
   later recurrence alerts immediately.
6. Pool identity/topology/geometry changes (`pool_key` mismatch) -> marker discarded,
   alert immediately. (Unchanged.)

## Design decisions

- **Marker shape:** `EnospcAck { pool_key: PoolKey, snoozed_until: u64 }`, where
  `snoozed_until` is the **Unix-epoch-seconds deadline** (ack time + interval). Flat
  `u64` (not serde's nested `SystemTime`, not RFC3339): the monitor compare is a plain
  integer compare, the on-disk file stays flat like today, and the VM test can rewrite
  the field with a one-line `jq`. `pool_key` is unchanged (fs_uuid + sorted
  `(devid, device_size)` pairs).
- **Interval:** `pub const ENOSPC_REMINDER_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60)`
  in `alert.rs` (next to `EnospcAck`; it is marker/ack policy, not capacity-byte math).
  No user-facing config knob: braid is opinionated, high-level UX (per AGENTS.md, "so
  people can run a NAS without fiddling with manpages"), and a reminder cadence is an
  internal policy detail -- not a product-boundary feature that "benefits from ...
  discoverability, or a unified config surface", which is principle 7's trigger for
  wrapping something in a `braid.*` option. A rarely-tuned reminder interval meets none of
  those triggers, so it stays a hardcoded constant. Documented in ADR 014.
- **Ack snoozes only a live risk.** `write_enospc_baseline` keeps the fresh ack-time
  `evaluate_enospc_risk(...)` assessment and writes the snooze marker **only when
  `assessment.at_risk()`**. Acking while the pool has recovered into the dead-band
  (`0 <= margin < ENOSPC_REARM_MARGIN`) would otherwise stamp a 7-day snooze onto a
  not-at-risk pool; the dead-band monitor branch *keeps* that marker, so a recurrence
  within the window would be wrongly suppressed -- violating contract #5. No live risk at
  ack time -> no marker -> recurrence fires armed. (Probe/parse failure and an absent
  fs_uuid already write no marker; this adds the not-at-risk guard.)
- **Snooze-window check (with a clock-anomaly bound):** suppress only while
  `now < snoozed_until <= now + interval`. The upper bound matters: a deadline further out
  than one interval means the clock moved between ack and now -- it ran ahead at ack time,
  *or* it was corrected backward (NTP/RTC fix) since a perfectly valid ack -- either way a
  structurally-valid file the corrupt-marker path would *not* catch. Treating it as
  elapsed bounds any clock anomaly to a single interval, fails toward reminding, and is
  trivially explainable ("a snooze lasts at most one reminder interval"). Encapsulate this
  in `EnospcAck`:

  ```rust
  // in alert.rs
  pub const ENOSPC_REMINDER_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

  fn unix_secs(now: SystemTime) -> u64 {
      now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
  }

  impl EnospcAck {
      /// Snooze ENOSPC reminders for one interval from `now`. Written by `braid ack`.
      pub fn snooze(pool_key: PoolKey, now: SystemTime) -> Self {
          EnospcAck {
              pool_key,
              snoozed_until: unix_secs(now)
                  .saturating_add(ENOSPC_REMINDER_INTERVAL.as_secs()),
          }
      }
      /// True while `now` is inside the snooze window: before the deadline and no
      /// more than one interval before it. A deadline beyond `now + interval` (the clock
      /// moved between ack and now -- ahead at ack, or corrected backward since) reads as
      /// elapsed so the monitor reminds again.
      pub fn is_snoozed(&self, now: SystemTime) -> bool {
          let now = unix_secs(now);
          now < self.snoozed_until
              && self.snoozed_until <= now.saturating_add(ENOSPC_REMINDER_INTERVAL.as_secs())
      }
  }
  ```

- **Time injection scope (low churn):** add `now: SystemTime` only to the two boundary
  functions -- `monitor::evaluate_enospc_for_monitor` and `ack::write_enospc_baseline`.
  Their single production callers (`cmd_monitor`, `cmd_ack_impl`) capture
  `SystemTime::now()` and pass it down. Do **not** thread `now` through `cmd_monitor` /
  `cmd_ack_impl` themselves -- that would force a new arg onto ~87 unrelated unit-test
  call sites. The exact boundary (`now == snoozed_until`, the clamp) is pinned by direct
  `EnospcAck::is_snoozed` unit tests; the wiring is pinned by the `cmd_monitor`-level
  integration tests (seed a past/future `snoozed_until`, real `now`).

## Code changes

### `cli/src/alert.rs`
- Add `use std::time::{Duration, SystemTime, UNIX_EPOCH};` (as needed).
- Add `ENOSPC_REMINDER_INTERVAL`, `unix_secs`, and the `EnospcAck::{snooze, is_snoozed}`
  impl above.
- Change `EnospcAck` to `{ pool_key: PoolKey, snoozed_until: u64 }` (drop
  `baseline_margin`). Keep derives, file name, and `save/load/remove_enospc_ack`.
- Rewrite the `EnospcAck` struct doc (it currently says "re-fires only when the live
  margin falls materially below `baseline_margin`") and the `save_enospc_ack` doc
  ("ack-time `margin`") to the snooze-deadline semantics.

### `cli/src/capacity.rs`
- Remove `ENOSPC_WORSEN_STEP` and its doc block. **Keep** `ENOSPC_REARM_MARGIN`
  (re-arm gate), `enospc_risk_threshold`, `evaluate_enospc_risk`, and `EnospcRiskAssessment`
  -- the monitor still needs `margin`/`at_risk()`/`count_below`/`device_count`.

### `cli/src/monitor.rs`
- Import: drop `ENOSPC_WORSEN_STEP`; keep `ENOSPC_REARM_MARGIN`, `evaluate_enospc_risk`;
  add `SystemTime`.
- `evaluate_enospc_for_monitor(...)`: add `now: SystemTime`. Replace the matching-key
  branch body (the `saturating_sub(ENOSPC_WORSEN_STEP ...)` compare) with:

  ```rust
  Some(live) if baseline.pool_key == *live => {
      if baseline.is_snoozed(now) { None } else { Some(cause) }
  }
  ```

  Rewrite the branch comment. **Leave unchanged:** degraded-skip-and-keep-marker,
  fail-open probe skip, re-arm (`margin >= ENOSPC_REARM_MARGIN` -> remove), dead-band
  (`!at_risk()` -> keep), no-baseline -> fire armed, corrupt -> fire armed + remove,
  confirmed key mismatch -> remove + fire armed, identity gap (no fs_uuid) -> fire
  armed + keep.
- `cmd_monitor`: capture `let now = SystemTime::now();` and pass to
  `evaluate_enospc_for_monitor`.

### `cli/src/ack.rs`
- **Keep** `use crate::capacity::evaluate_enospc_risk;` (still needed for the at-risk gate).
- `write_enospc_baseline`: add `now: SystemTime`; **keep** the `missing_count` param and
  the `evaluate_enospc_risk(&entries, missing_count)` assessment. Save
  `EnospcAck::snooze(pool_key, now)` **only when `assessment.at_risk()`**; otherwise write
  no marker (the caller clears the latch regardless). The probe + `live_pool_key` +
  no-fs_uuid / probe-failure best-effort guards stay (each already writes no marker).
- `cmd_ack_impl`: **keep** the `missing_count` it computes; capture
  `let now = SystemTime::now();` and call
  `write_enospc_baseline(runner, mount_point, &pool, missing_count, paths, now)`.

## Tests

Maps every required scenario to a concrete test. Project rule: each VM test keeps its
Intent/Why/Scenario preamble; flake `checks` registration is unchanged (we extend the
existing VM test, add no new `.nix`).

### Rust unit tests
- **`alert.rs` (new):**
  - `enospc_ack_snooze_sets_deadline_one_interval_out` -- `EnospcAck::snooze(k, t).snoozed_until == unix_secs(t) + ENOSPC_REMINDER_INTERVAL.as_secs()`.
  - `enospc_ack_is_snoozed_window` -- with a fixed `now`: snoozed just-before deadline
    (true), exactly at deadline (false -> remind), elapsed/past (false), clamp upper
    boundary `snoozed_until == now + interval` (true -- the freshly-written deadline; pins
    the clamp's inclusive `<=`), and just past the clamp `> now + interval`
    (false -> remind). *Covers the window boundary + clock-anomaly bound deterministically;
    the `==` and `>` cases together pin `<=` against a slip to `<`.*
  - `enospc_reminder_interval_is_seven_days` -- pins `ENOSPC_REMINDER_INTERVAL == Duration::from_secs(7*24*60*60)`.
  - Update `enospc_ack_save_load_remove_roundtrip` to the new field (and assert the
    flat on-disk shape `{ "pool_key": {...}, "snoozed_until": <int> }`).
- **`capacity.rs`:** rewrite `enospc_hysteresis_constants_pinned` -> pin only
  `ENOSPC_REARM_MARGIN == 1 GiB` (drop the `ENOSPC_WORSEN_STEP` asserts). Update the
  "materially worse re-alert" comment on `evaluate_enospc_risk_margin_is_monotonic_in_severity`
  to reference re-arm only (margin monotonicity still gates re-arm); keep the test.
- **`monitor.rs`:** change the `seed_enospc_baseline` helper to take `snoozed_until: u64`.
  - Rework `cmd_monitor_suppresses_enospc_while_acked_not_worse` ->
    `cmd_monitor_suppresses_enospc_within_snooze`: seed `snoozed_until = now + (interval/2)`,
    at-risk usage, assert `MonitorResult::Ok` + marker kept. *(matching key before expiry suppresses)*
  - Rework `cmd_monitor_refires_enospc_when_materially_worse` ->
    `cmd_monitor_refires_enospc_after_snooze_elapsed`: seed `snoozed_until = 1` (elapsed),
    at-risk usage, assert it fires `EnospcRisk`. *(matching key after expiry re-alerts)*
  - **New** `cmd_monitor_after_reack_is_snoozed_again`: seed elapsed marker -> `cmd_monitor`
    fires -> `cmd_ack` -> `cmd_monitor` returns `Ok` (re-ack reset the window). *(re-ack resets interval)*
  - **New** `cmd_monitor_at_risk_no_marker_fires_armed`: no `enospc-ack.json` present,
    at-risk payload -> `cmd_monitor` fires `EnospcRisk` armed. Pins the
    "recurrence after a healthy ack alerts immediately" half of contract #5 (F1).
  - **New** `cmd_monitor_old_margin_shaped_marker_fires_armed_and_clears`: write a raw,
    structurally-valid old-shape `{ "pool_key": {<matching key>}, "baseline_margin": <i64> }`
    to `enospc-ack.json`, run an at-risk cycle; assert it fires `EnospcRisk` **and removes**
    the marker (the missing `snoozed_until` makes `load_enospc_ack` return `Err` -> corrupt
    path). Pins the "no migration; stale margin-shaped file -> corrupt -> fire armed + clear"
    claim against a future `#[serde(default)]` slip (F3).
  - Update the seed calls (to a future `snoozed_until`) in the preserved tests:
    `cmd_monitor_rearms_on_predicate_health_then_refires` *(risk clears past REARM, marker removed; recurrence fires)*,
    `cmd_monitor_stale_baseline_key_mismatch_fires_and_clears` *(key mismatch clears + fires armed)*,
    `cmd_monitor_identity_gap_fires_armed_and_keeps_baseline` *(identity gap fires + keeps marker)*,
    `cmd_monitor_degraded_skips_enospc_and_preserves_baseline` *(degraded skips + keeps marker)*.
    Unchanged in spirit: `cmd_monitor_corrupt_baseline_fires_armed_without_computation_error`
    *(corrupt fires armed + clears)*, `cmd_monitor_enters_enospc_risk_warns_without_beep`
    *(first risk fires Warning)*, `cmd_monitor_usage_probe_failure_*` *(probe failure skips only EnospcRisk)*.
- **`ack.rs`:** rework `cmd_ack_mounted_enospc_risk_writes_reprobed_keyed_baseline`:
  bracket `SystemTime::now()` around the call and assert the written marker has the
  correct `pool_key` and `snoozed_until` in `[before + interval, after + interval]`
  (exact arithmetic is covered by `EnospcAck::snooze` unit test). Keep
  `..._unstubbed_usage_writes_no_baseline` and `..._no_fs_uuid_writes_no_baseline`.
  Remove the `ack.baseline_margin == ACK_ATRISK_MARGIN` assertion; if `ACK_ATRISK_MARGIN`
  is then unused, drop it from the `ack.rs` import (and the const/re-export if fully dead).
  - **New** `cmd_ack_mounted_enospc_healthy_at_ack_writes_no_snooze`: latch carries
    `EnospcRisk`, but the fresh ack-time usage probe is healthy (dead-band, `0 <= margin
    < REARM`); assert `cmd_ack` clears the latch and writes **no** `enospc-ack.json`. The
    at-risk-gate regression for F1 (pairs with `cmd_monitor_at_risk_no_marker_fires_armed`).

### VM test -- extend `tests/cli/braid-monitor-enospc.py`
The current flow (fill -> exit 3 -> advisory routed, no beep -> ack writes marker ->
follow-up monitor exit 0) stays. After the "Acked-but-still-at-risk -> exit 0" subtest
(reword its comment: suppressed **within the snooze window**, not forever), insert:

- **"Ack snoozes but does not resolve -- status still shows the live advisory":**
  immediately after the reworded within-window exit-0 subtest (a real `braid ack` has
  already written the marker), assert `braid status` still contains `"ENOSPC risk"`. This
  pins the headline snooze-not-resolve contract end-to-end -- ack snoozes the reminder, it
  does not resolve the risk, and `braid status` keeps surfacing the live advisory while
  the marker exists. Without it the promise rests only on `status.rs` happening not to
  read `enospc-ack.json`; a future change that made status suppress the advisory while a
  marker exists would pass every existing test (`build_status_warns_on_enospc_risk` seeds
  no marker; the VM checks status only *before* ack). Chosen over a `status.rs` unit test
  seeding an `EnospcAck` because the VM line is one assertion in territory this plan
  already edits and it exercises the real ack.
- **"Reminder elapses -> monitor re-alerts (exit 3)":** rewrite the deadline to the
  past, preserving `pool_key` (jq is in the VM via `pkgs.jq`):
  `tmp=$(mktemp); jq '.snoozed_until = 1' /var/lib/braid/enospc-ack.json > "$tmp" && mv "$tmp" /var/lib/braid/enospc-ack.json`,
  then `braid monitor` exits 3 and the latch reappears. (Timer already stopped at the
  top of the test, so nothing races the edit.)
- **"Re-ack snoozes for another interval -> exit 0":** `braid ack`, then `braid monitor`
  exits 0; assert `snoozed_until` is now in the future (proves re-ack reset the deadline,
  not a vacuous pass). Then the existing degraded-pool subtest follows unchanged.

Update the preamble Intent to mention the snooze/reminder cycle.
`braid-monitor-enospc-geometry.py` is **unchanged** -- its re-fire is driven by
`pool_key` mismatch, which this change does not touch.

## Docs

- **`docs/design/decisions/014-alerts.md`** (authority; its current text actively
  contradicts the new model):
  - **Rewrite the "Severity tiers and the ENOSPC baseline" body.** Replace the "monotonic
    risk-magnitude baseline" / "Ack records the ack-time margin" / "Re-alert only if
    materially worse" / "hysteresis gap between the worsen step and the re-arm margin"
    bullets with the snooze/reminder model (ack writes a deadline one
    `ENOSPC_REMINDER_INTERVAL` = 7 days out; the monitor suppresses only inside the window
    and re-fires **every cycle** once it elapses until re-ack; a deadline beyond
    now+interval is treated as elapsed because the clock moved between ack and now; re-arm
    on clear is unchanged; written only by ack, removed only by the monitor). State that
    ack = snooze, not resolve, and that `braid status` shows the live advisory regardless.
    Note: no on-disk migration; a stale margin-shaped marker deserializes as corrupt ->
    fire armed + clear.
  - **Keep the heading text "Severity tiers and the ENOSPC baseline" unchanged** -- it is a
    stable anchor, not a rename target. Its slug has **four** referrers: the in-file
    self-link at the `EnospcRisk` cause definition, plus three in the archived
    `plans/impl/2026-06-22-alert-on-scrub-failure.md` (a relative markdown link carrying the
    anchor, and two prose slug references). `plans/` is outside the mdBook `src` tree, so
    `mdbook-linkcheck2` / `just docs-build` would *not* catch a dangling anchor if the
    heading moved -- a rename would silently break the relative link and force churn into a
    point-in-time impl record. So "baseline" survives only as the heading's anchor label /
    the `enospc-ack.json` file's historical nickname; add a one-line inline note to that
    effect so the heading does not read as if the margin-baseline model still applies.
  - **Preserve the `pool_key` identity paragraph's substance** (the `device_size`
    rationale and the no-fs_uuid identity-*gap* behavior are unchanged) but rename its
    "baseline" nouns to "snooze marker"/"marker" to match the swept body prose (only the
    heading anchor keeps "baseline").
  - **Sweep the "Offline ack policy" `EnospcRisk` bullet** -- the other place the marker is
    described. Reword "the next mounted ack establishes the keyed **baseline**" to "the
    next mounted ack **snoozes** it (writes the reminder deadline)", and drop "**once**":
    the latch is sticky-until-ack, so an offline-acked, still-at-risk pool re-fires
    `EnospcRisk` on the next and each subsequent mounted cycle until a mounted ack snoozes
    it. (Offline ack still clears the latch and writes no marker -- unchanged.)
  - **Sweep the fail-closed carve-out wording**: the best-effort ENOSPC probe's
    "baseline-load failure" becomes "marker-load failure" (it loads `enospc-ack.json`).
  - **Scope guard:** sweep only the *ENOSPC* (`enospc-ack.json`) "baseline" terminology in
    the body prose. Leave the `acked-stats.json` **device-error** baseline terminology untouched
    (`BtrfsDeviceErrors`, the acked-stats-hygiene section, the offline
    `MissingDevice`/`BtrfsDeviceErrors` bullets) -- a real, unchanged baseline and a
    different concept.
- **`docs/commands/monitor.md`** -- "What triggers an alert" ENOSPC bullet: replace
  "it re-fires only if the pool gets materially worse, and re-arms when the risk clears"
  with the snooze/reminder wording (ack snoozes; reminds again after the interval if
  still at risk; ack again to re-snooze; re-arms immediately when risk clears).
- **`docs/commands/ack.md`** -- currently documents only the `acked-stats.json`
  device-counter baseline (lead sentence + "What happens under the hood" step 2) and never
  mentions ENOSPC. Add: in the mounted section, a latched `EnospcRisk` whose fresh probe is
  still at risk also writes a 7-day reminder deadline to `enospc-ack.json` -- a *snooze*,
  not a baseline reset, and no marker if the risk has cleared by ack time. In the offline
  section, an offline `EnospcRisk` ack clears the latch but writes no reminder marker
  (offline cannot probe `pool_key` / confirm risk), so it re-fires on remount and each
  subsequent mounted cycle until a mounted ack snoozes it (matching the corrected ADR 014
  wording -- the latch is sticky-until-ack, not a one-shot). Reconcile the lead sentence so
  it does not imply ack only baselines device-error counters.
- **`docs/design/decisions/018-systemd-lifecycle.md`** -- exit-code table is unchanged;
  add at most one sentence noting the advisory may re-fire on later cycles per ADR 014's
  reminder interval (cadence is owned by ADR 014).
- **`docs/guides/monitoring-and-alerts.md`** -- conceptual update flagged in the brief:
  it still reads as if all alerts beep until ack and ack "resets the baseline." Clarify
  ENOSPC is a non-beeping advisory you *snooze* (7-day reminder), distinct from the
  beeping Critical alerts.
- **`docs/commands/status.md`** -- review the ENOSPC advisory section; light edit only
  if it implies ack resolves the risk (status keeps showing it).
- **`README.md`** -- update the monitoring bullet if it summarizes the old re-alert rule.

ASCII-only in all CLI output / `.nix` echo lines (no change expected here; gate below).

## Verification

```sh
just test-rust
just test-parsers
nix build .#checks.aarch64-darwin.braid-monitor-enospc
nix build .#checks.aarch64-darwin.braid-monitor-enospc-geometry
just docs-build
scripts/docs/check-output-ascii.py
```

- `test-rust` must show the reworked/new unit tests passing (snooze arithmetic, window
  boundary + clamp, within-snooze suppress, post-expiry re-fire, re-ack reset, ack
  writes future deadline).
- The `braid-monitor-enospc` VM check must pass through the new reminder-elapses /
  re-ack subtests; `braid-monitor-enospc-geometry` must still pass unchanged.
- `docs-build` validates the ADR/guide cross-links; the ASCII gate covers CLI strings.

## Out of scope / invariants preserved

ENOSPC stays Warning (exit 3), non-beeping, ackable. `pool_key` (fs_uuid + sorted
`(devid, device_size)`) and all its mismatch/identity-gap/corrupt handling are
unchanged. Fail-open is unchanged: a usage-probe/parse/marker-load failure skips only
`EnospcRisk` and never latches `ComputationError`. `braid status`'s live advisory and
the `enospc_risk_advisory` predicate are untouched. No new config knob. No on-disk
migration.

## Implementation notes

- **`cmd_monitor_after_reack_is_snoozed_again` drives the real ack via a now-`pub(crate)`
  `cmd_ack_impl`, not `cmd_ack`.** The plan said "-> `cmd_ack` ->", but `cmd_ack` shells
  out to `systemctl stop` (the exact reason `cmd_ack_impl` exists -- ack.rs already
  documents that tests use the injectable-hook variant so they never touch host systemd).
  Calling `cmd_ack` from a unit test would regress that discipline. So `cmd_ack_impl` is
  now `pub(crate)` and the monitor test calls it with the existing `ack_noop_beeper` hook,
  exercising the real ack -> monitor handoff (probe, at-risk gate, fresh deadline) without
  shelling out. The monitor + ack fixture worlds already cross (ack tests import
  `cmd_monitor`/`MonitorTestRunner`), so this adds no new coupling shape.
- **Dead-band fixture for the healthy-at-ack gate.** `ack_btrfs_device_usage_healthy`
  seeds device 1 at 1.5 GiB unallocated against the 1 GiB threshold -> predicate margin
  0.5 GiB, i.e. `0 <= margin < ENOSPC_REARM_MARGIN` (the dead band), which is the case
  contract #5 cares about (the monitor *keeps* a dead-band marker, so a snooze wrongly
  written there would suppress a recurrence).
- **"baseline" survives as nicknames, by the plan's own decision.** The heading anchor
  `severity-tiers-and-the-enospc-baseline` (four referrers), the `enospc-ack.json` file
  nickname, and a few unit-test names (`enospc_hysteresis_constants_pinned`,
  `cmd_ack_mounted_enospc_risk_writes_reprobed_keyed_baseline`) keep "baseline" rather
  than churn names/anchors; the swept body prose uses "snooze marker"/"marker".
- **README needed no change.** Its monitoring bullet only says ENOSPC "raises a quieter
  non-beeping warning" -- it never summarized the old "materially worse" re-alert rule,
  so there was nothing to update.
