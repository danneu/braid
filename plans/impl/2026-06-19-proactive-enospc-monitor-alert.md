# Plan: push ENOSPC diagnostics through the alert path

## Context

braid already *detects* btrfs RAID1 chunk-exhaustion (ENOSPC) risk well, but only
on demand: `cli/src/capacity.rs#enospc_risk_advisory` is computed when a human runs
`braid status` (`cli/src/status.rs#build_status`) or `braid doctor`
(`cli/src/doctor.rs#check_enospc_risk`). An unattended NAS that is slowly filling
never proactively warns anyone -- the first signal is an actual allocation failure.

The periodic alert path (`braid-monitor` timer -> `cli/src/monitor.rs#cmd_monitor`
-> `cli/src/alert.rs#compute_alert_state` -> `braid-alert.service`) currently only
watches monotonic device-error counters, missing devices, and the smartd flag
(ADR 014). This change makes `braid monitor` also raise a proactive alert when the
pool approaches RAID1 chunk ENOSPC, reusing that path rather than bolting on a
parallel one.

### Decisions locked in (with the user)

1. **Severity-tiered notification.** `AlertCause` gains a severity. ENOSPC risk is
   `Warning`; the existing data-loss/redundancy causes stay `Critical`. The audible
   beep is reserved for `Critical`. A `Warning`-only cycle notifies via the
   user `alertCommand` + `braid status`, but does **not** beep. This fixes the
   correctness bug of training the operator to mute the channel ADR 014 built for a
   dying disk.
2. **Monotonic risk-magnitude baseline** for ack/re-alert, mirroring the existing
   `cli/src/alert.rs#exceeds_acked` shape: ack records the current risk magnitude;
   re-alert only if it gets materially worse; re-arm when the risk clears so a
   future recurrence alerts fresh. Stored in a dedicated state file, not in
   `acked-stats.json`.
3. **Two distinct causes, chunk-pair first.** This change ships the chunk-pair
   `EnospcRisk` cause. Metadata pressure (`doctor.rs#check_metadata_enospc_pressure`)
   is a genuinely different condition (different probe, opposite remediation) and is
   deferred to a sibling cause -- see [Deferred](#deferred--open-questions).

### The non-optional spine

- **One typed predicate.** Factor the *decision* out of `enospc_risk_advisory`'s
  prose so `status`, `doctor`, and `monitor` consume one source of truth and the
  thresholds are defined once.
- **Scoped fail-open probe.** The monitor's new `btrfs device usage` probe is
  best-effort: a probe/parse failure skips *only* the `EnospcRisk` cause, never masks
  device-error/missing-device alerting, and never latches `ComputationError`. This is
  the single documented exception to the monitor's fail-closed mandate -- named in
  ADR 014's pure-detector contract (the `ComputationError` cause-taxonomy authority
  that ADR 018 defers to), with the probe mechanism in ADR 018.
- **Reuse the degraded guard.** Skip `EnospcRisk` whenever the alert pipeline's
  `AlertDevids.missing` is non-empty (the same source that drives `MissingDevice`),
  so degraded pools and planned add/remove/replace do not false-positive.

## Design: the `EnospcRisk` cause, severity, and baseline state machine

**New cause** (`cli/src/alert.rs#AlertCause`):
`EnospcRisk { margin: i64, count_below: u32, device_count: u32 }`. `margin` is the
predicate's binding signed surplus/deficit in bytes (negative == at-risk depth,
positive == healthy surplus); it is the *displayed* risk magnitude. The other two
fields render the status/advisory line without a re-probe. Serializes as
`{"type":"enospc_risk", ...}` (internally tagged, snake_case -- consistent with
existing variants). The cause deliberately carries **no** pool-identity/`pool_key`
data: keying lives in `enospc-ack.json` (written by `braid ack` from a fresh probe,
below), so the public `status --json` cause stays a clean risk descriptor and is not
coupled to the internal baseline-keying scheme. The suppression baseline is the
*ack-time* margin, captured by `braid ack` from its own probe (mirroring
`snapshot_current`), not this latched fire-time value.

**Severity** (`cli/src/alert.rs`): new `enum AlertSeverity { Warning, Critical }`
with `Critical > Warning`. `AlertCause::severity()` returns `Warning` for
`EnospcRisk` and `Critical` for `BtrfsDeviceErrors`, `MissingDevice`, `SmartdAlert`,
`ComputationError` (`ComputationError` is fail-closed/indeterminate -> must beep).
`AlertState::severity() -> Option<AlertSeverity>` returns the max over `causes`
(`None` when empty).

**Monitor state machine** (pool-global, persisted in `enospc-ack.json`; absent =
"armed, not acked"). Each `cmd_monitor` cycle, after device-stats/missing/smartd:
- Skip entirely (emit nothing, leave any baseline untouched) if the pool is degraded
  (`AlertDevids.missing` non-empty).
- Compute the predicate's signed `margin` via `evaluate_enospc_risk` (negative =
  deficit, positive = healthy surplus); `at_risk == margin < 0`.
- Build the live `Option<PoolKey>` (see below); it is `None` only when the pool's
  `fs_uuid` is absent. Load the baseline. It is *usable* only when (a) the live key is
  `Some`, (b) it loads cleanly, **and** (c) its stored `pool_key` equals the live key.
  Remove the stale file (`remove_enospc_ack`) only when the baseline is *positively*
  invalid -- corrupt/unreadable, or a clean baseline whose key **differs** from a
  `Some` live key. When we merely **cannot compare** (live key `None`, an identity
  gap, not a confirmed different pool), treat as no usable baseline for this cycle but
  **leave the file in place** -- a later cycle with `fs_uuid` present compares and
  re-arms it properly.
- If `at_risk` and no usable baseline (armed): emit `EnospcRisk { margin, .. }`.
- If `at_risk` and a usable baseline `B` (acked): emit only if
  `margin < B.baseline_margin - ENOSPC_WORSEN_STEP` (materially worse, i.e. more
  negative); else suppress.
- If healthy by the predicate's own surplus (`margin >= ENOSPC_REARM_MARGIN`):
  `remove_enospc_ack` (re-arm). Emit nothing. (Re-arm keys off the predicate margin,
  **not** raw min-headroom, so a predicate-healthy pool with one low device still
  re-arms.)
- The baseline is **written only by `braid ack`** (live `pool_key` + the *ack-time*
  `margin`, both from one fresh `btrfs device usage --raw` probe) and **removed only
  by the monitor** (re-arm, confirmed key mismatch, or corruption). Hysteresis
  (`ENOSPC_REARM_MARGIN`, ~1 data chunk) prevents boundary flapping; the monotonic
  compare needs no separate pre-ack hysteresis because `merge_into_latch` already
  latches the first fire until ack.

**Baseline identity (`pool_key`).** `EnospcAck` stores a `pool_key` -- the btrfs
filesystem UUID plus the sorted per-device `(devid, device_size)` pairs (from the
`btrfs device usage --raw` rows the monitor and ack already parse) -- captured at ack
time. The monitor invalidates a baseline whose key differs from the live pool, so a
baseline acked on an old pool (bootstrap/recreate -> new FS UUID), an old membership
(add/remove -> changed devid set), **or** an old geometry (`braid replace`/resize ->
same devid, changed `device_size`) cannot suppress a fresh `EnospcRisk`. Keying on
`device_size`, not just devid, is what closes the same-devid replace gap: `btrfs
replace` keeps the source devid (`cli/src/replace.rs#ReplacePlan::execute` runs
`btrfs replace start <devid>` then `pool_resize_device`), so `fs_uuid + devids` alone
would stay identical while the chunk-pair capacity geometry the predicate depends on
changed. `device_size` is stable across normal fill (only `used`/`unallocated` move),
so the key does **not** churn while a pool is merely filling -- it changes only on a
real topology/geometry event. This is the `EnospcRisk` analog of the membership-change
hygiene that `acked-stats.json` gets from `cli/src/alert.rs#reconcile_acked_stats` +
the `add`/`remove`/`recover` ghost-drop callers (ADR 014
`#acked-stats-hygiene-across-pool-membership-changes`); keying the baseline is
self-validating, so a missed command hook cannot reintroduce the stale-baseline class.

This deliberately differs from ADR 014's "latched until ack even if the condition
disappears" only in the *post-ack* baseline (re-arm on clear), exactly as
`MissingDevice` + `missing_acked` already self-re-arm via
`cli/src/alert.rs#reconcile_acked_stats`. The *latch itself* stays sticky-until-ack
(merge carries it forward), so this is consistent with the invariant, not a break.

## Implementation steps (TDD: write the failing test first at each step)

### Step 0 -- unblock the monitor test fixtures (mechanical, do first)

Adding a `BtrfsDeviceUsageRaw` probe to `cmd_monitor` makes every existing monitor
test panic, because `cli/src/test_fixtures/monitor.rs#MonitorTestRunner::run` and
`MonitorReconcileRunner::run` `panic!` on any unhandled `CmdRequest`.

- Add a `CmdRequest::BtrfsDeviceUsageRaw { .. } =>` arm to **both** runners, returning
  a healthy `btrfs device usage --raw` payload by default (new `USAGE_2DISK_HEALTHY`
  const; model the raw format from the `parse_btrfs_device_usage` fixtures and
  `cli/src/test_fixtures/status.rs`).
- Add `MonitorOverride::UsageResult(Result<RawCommandOutput, CmdError>)` (+ a
  `take_usage_result` accessor) and an at-risk payload const `USAGE_2DISK_ATRISK`,
  so tests can inject at-risk usage and probe failures. The usage-payload builder must
  let a test vary `device_size` (and devid/`fs_uuid`) independently of `unallocated`,
  so the keying tests can model a same-devid `replace`/resize (same devids, changed
  `device_size`) and a missing-`fs_uuid` probe.
- Add a combined constructor `with_stats_payload_and_usage(stats, MonitorOverride)`.
  Today `with_stats_payload(..)` sets `override_op: None` and `with_override(..)` resets
  `stats_payload` to the healthy default (`MonitorTestRunner` holds one stats field +
  one one-shot override slot), so **no** existing constructor expresses
  "device-error stats **and** a usage-probe `Err` at once" -- exactly the
  probe-failure-isolation test's precondition. Stats come from the `stats_payload`
  field and usage from the override slot, so one combined constructor (custom
  error-bearing stats + `UsageResult(Err)`) suffices; the single override slot is not a
  blocker because stats need no override.

### Step 1 -- typed predicate (`cli/src/capacity.rs`)

- Add `pub struct EnospcRiskAssessment { pub margin: i64, pub count_below: usize,
  pub device_count: usize, pub threshold: u64 }` with `fn at_risk(&self) -> bool {
  self.margin < 0 }`.
- Add `pub fn evaluate_enospc_risk(devices: &[BtrfsDeviceUsageEntry],
  missing_count: u64) -> EnospcRiskAssessment`. Lift the decision out of
  `enospc_risk_advisory`: the `missing_count > 0 || devices.len() < 2` guard, the
  2-device branch, and the 3+-device per-loss simulation (reusing
  `enospc_risk_threshold` and `raid1_chunk_pair_capacity`). Define `margin` as the
  binding *signed* predicate-relative surplus -- the **same** quantity that decides
  `at_risk`, just kept signed instead of collapsed to a bool:
  - 2-device branch: `min` over devices of `unallocated - current_threshold`.
  - 3+-device branch: `min` over single-disk-losses of
    `raid1_chunk_pair_capacity(survivors) - survivor_threshold`.

  So `margin < 0` is exactly the existing `at_risk` predicate (negative = deficit /
  at-risk depth; positive = healthy surplus). This is a strict generalization: status
  and doctor see identical decisions, and the monitor gets the surplus it needs to
  re-arm correctly. The degraded / single-disk guard returns a sentinel healthy margin
  (e.g. `i64::MAX`-capped) so callers treat it as not-at-risk.
- Rewrite `enospc_risk_advisory` as a thin formatter over `evaluate_enospc_risk`,
  preserving the **exact** current string (status/doctor and their tests are
  unchanged). `doctor.rs#check_enospc_risk` keeps calling `enospc_risk_advisory`, so
  doctor needs no change.
- Add `pub const ENOSPC_WORSEN_STEP: u64` and `ENOSPC_REARM_MARGIN: u64` (~1 GiB,
  one data chunk); pin exact values via tests.

### Step 2 -- cause, severity, latch keying, baseline I/O (`cli/src/alert.rs`, `cli/src/state_paths.rs`)

- Add the `EnospcRisk` variant to `AlertCause` and the `AlertSeverity` enum +
  `AlertCause::severity()` + `AlertState::severity()`.
- Add the singleton arm to `cli/src/alert.rs#same_cause_key`:
  `(EnospcRisk{..}, EnospcRisk{..}) => true` (mandatory -- the `_ => false`
  fallthrough would otherwise append a duplicate each cycle).
- Do **not** modify `compute_alert_state` (it has no usage input; `EnospcRisk` is
  computed in `cmd_monitor`).
- Add baseline state:
  - `pub struct PoolKey { pub fs_uuid: String, pub devices: Vec<(Devid, u64)> }`
    (sorted by devid; each pair is `(devid, device_size)`) -- the pool identity *and
    geometry* the baseline is bound to. Including `device_size` is what makes a
    same-devid `braid replace`/resize invalidate the baseline (F1).
  - `pub struct EnospcAck { pub baseline_margin: i64, pub pool_key: PoolKey }`.
  - `load_enospc_ack(paths) -> Result<Option<EnospcAck>, ...>` -- **tri-state and
    fallible**, NOT lossy: `Ok(None)` for an absent file, `Ok(Some(_))` for a clean
    parse, `Err(_)` for read/parse failure. The monitor decides what to do with each
    (see Step 3); resolving the corrupt case is the monitor's job, not the loader's,
    so the policy lives in one place.
  - `save_enospc_ack`, `remove_enospc_ack` (NotFound tolerant) -- mirroring
    `save_acked_stats` / `remove_acked_stats` and `atomic_write`.
- Expose the pool identity the key needs: add `pub fs_uuid: Option<String>` to
  `cli/src/probe.rs#AlertPoolState`, populated from the btrfs filesystem UUID the probe
  already parses (`show.uuid`; the monitor/ack show fixtures already carry a `uuid:`
  line). Add a constructor `fn live_pool_key(fs_uuid: Option<&str>, devices:
  &[BtrfsDeviceUsageEntry]) -> Option<PoolKey>` that returns `Some` only when
  `fs_uuid` is present (building `devices` as sorted `(e.devid, e.device_size)` from
  the usage rows) and **`None` when `fs_uuid` is absent**. There is no membership-only
  fallback: a `None` live key means "no usable identity", and both consumers treat it
  as no-usable-baseline -- the monitor may fire if at risk (it does not invent a key
  that could spuriously match a stored one), and ack writes no baseline. This removes
  the prior draft's self-contradiction (it built a weaker fallback key *and* declared
  it unusable); the strongest identity field being absent must not silently weaken the
  guard.
- `cli/src/state_paths.rs`: add `pub fn enospc_ack_json(&self) -> PathBuf` next to
  `acked_stats_json`/`alert_latch_json`, and add it to the path list used by the
  cleanup/test helpers in that file.

### Step 3 -- monitor probe + state machine (`cli/src/monitor.rs`)

- Add `fn evaluate_enospc_for_monitor<R: CommandRunner>(runner: &R,
  mount_point: &MountPoint, missing_count: u64, fs_uuid: Option<&str>,
  paths: &StatePaths) -> Option<AlertCause>` returning `Some(EnospcRisk{..})` to fire
  or `None` to suppress. The helper probes usage itself and builds the live key
  internally (`live_pool_key(fs_uuid, &entries)`) -- the caller does not have the usage
  rows. It distinguishes the failure modes the prior draft conflated:
  - **Cannot determine risk** (usage probe `Err`, or `parse_btrfs_device_usage`
    `Err`): `eprintln!` and return `None` -- skip the cause for this cycle. This is the
    scoped fail-open exception; it never latches `ComputationError`, so device-error /
    missing-device alerting in the same cycle is untouched.
  - **Risk is known, baseline is positively invalid** (`load_enospc_ack` returns
    `Err`, or returns `Ok(Some)` whose `pool_key` differs from a `Some` live key): we
    *can* evaluate risk, the stored baseline is confirmed stale/corrupt. Treat as
    **no usable baseline (armed)**, `eprintln!`, best-effort `remove_enospc_ack`, and
    continue -- so a corrupt/stale baseline cannot silently suppress a real risk.
  - **Risk is known, identity is not** (live key `None` because `fs_uuid` is absent):
    we cannot compare, but this is an identity *gap*, not a confirmed different pool.
    Treat as **no usable baseline (armed)** and `eprintln!`, but do **not** remove the
    stored file -- a later cycle with `fs_uuid` present compares and re-arms it.
  - A clean baseline whose key **matches** the `Some` live key drives the monotonic
    compare.
  Then apply the
  [state machine](#design-the-enospcrisk-cause-severity-and-baseline-state-machine)
  to decide fire vs suppress, and re-arm (`remove_enospc_ack`) when
  `margin >= ENOSPC_REARM_MARGIN`.
- In `cmd_monitor#classified`, after step 7 (`compute_alert_state`), change
  `let live_causes` to `let mut live_causes`, compute
  `let missing_count = devids.missing.len() as u64;`, then
  `if let Some(c) = evaluate_enospc_for_monitor(runner, mount_point, missing_count,
  pool.fs_uuid.as_deref(), paths) { live_causes.push(c); }` (the helper builds the key
  from its own usage parse + this `fs_uuid`). Keep this on the fail-open side: the
  helper owns its errors and never returns `Err`, so it never enters the
  `?`-propagating fail-closed `ComputationError` path. Add a comment citing the
  fail-closed carve-out (ADR 014's pure-detector contract names it; ADR 018 documents
  the probe mechanism).

### Step 4 -- severity-tiered notification (`cli/src/main.rs`, `modules/braid/monitor.nix`)

- `cli/src/main.rs` (the `cmd_monitor` match, currently `MonitorResult::Alert(_) =>
  exit(1)`): branch on `state.severity()` -- `Critical => exit(1)`,
  `Warning => exit(3)`, and `None => exit(1)` (**fail-closed**). `severity()` is
  `Option` because an empty `AlertState` has no max; a `MonitorResult::Alert` always
  carries >=1 cause so `None` is unreachable in practice, but the arm must exist and
  must default to the beeping exit, never a silent exit 0. Exit 3 is already an
  established convention in this file.
- `modules/braid/monitor.nix`:
  - Wrapper script: add `elif [ "$rc" -eq 3 ]; then systemctl start
    braid-alert-advisory.service` **before** the existing `elif [ "$rc" -ge 2 ]`
    failure branch (so 3 is not misread as a failure).
  - New `systemd.services.braid-alert-advisory`: `Type = "oneshot";
    RemainAfterExit = true;` running only `alertCommand` (`|| true`) -- no beep, no
    loop. `RemainAfterExit` makes repeated 5-minute `systemctl start` cycles a no-op
    until ack stops it, so `alertCommand` is not re-run every cycle (mirrors the
    existing no-beep `braid-alert.service` form).
  - The `Critical`/exit-1 path (`braid-alert.service`) and the smartd hook are
    unchanged (SMART stays `Critical`).

### Step 5 -- ack baseline + offline policy (`cli/src/ack.rs`)

- **Mounted ack** (`cmd_ack_impl`, which already re-probes current state -- it runs
  `BtrfsDeviceStatsJson` then `snapshot_current`): when the latched `causes` contain
  `EnospcRisk`, issue one **best-effort** `CmdRequest::BtrfsDeviceUsageRaw` probe (the
  same request `status.rs#get_device_usage` uses), parse it, build the live
  `Option<PoolKey>` via `live_pool_key(pool.fs_uuid.as_deref(), &entries)`, and compute
  the *current* `margin` via `evaluate_enospc_risk`. If the key is `Some`,
  `save_enospc_ack(EnospcAck { baseline_margin: <current margin>, pool_key })`. This
  re-probe (rather than reading the latched fire-time `margin`) mirrors
  `snapshot_current`: the baseline is the state the operator *acknowledged*, and key +
  margin come from one coherent instant, so a pool that filled further between fire and
  ack is not immediately re-fired. The probe is **best-effort**: if the usage probe or
  parse fails, **or** the live key is `None` (no `fs_uuid`), ack still clears the latch
  but writes **no** baseline (log; same end-state as offline ack -- one quiet re-fire
  next cycle, then a clean ack establishes it). Because `MockRunner` returns
  `Err(CmdError::MissingMock)` for an unconfigured request, existing ack tests that do
  not stub usage simply take the no-baseline path and stay green; only the new
  enospc-ack tests stub the usage payload.
- The baseline must **persist** past ack (it is the post-ack suppression memory), so
  do **not** add `enospc_ack_json` to `cleanup_alert_files_and_beeper` -- only the
  monitor removes it (re-arm, key mismatch, or corruption).
- **Offline ack** (`ack_offline`): `EnospcRisk` carries no monotonic counter, so it is
  **allowed** offline (do not add it to the `OfflineBtrfsErrorsRefused` guard) -- it
  clears the latch like `MissingDevice`/`SmartdAlert`. But it **writes no baseline**:
  offline ack cannot probe the live `pool_key`, and a keyless baseline would just be
  invalidated anyway. Consequence: if the pool remounts still at-risk, the monitor
  re-fires `EnospcRisk` once at the quiet warning level (exit 3, no beep), and the next
  mounted ack establishes the proper keyed baseline. This is acceptable for a
  non-beeping advisory and avoids an offline dependency on `pool.json` membership for
  the key; see [Deferred](#deferred--open-questions) if durable offline suppression is
  wanted later.
- Extend the production stop hook (`ack.rs#stop_beeper`) to also
  `systemctl stop braid-alert-advisory.service`, so ack silences the advisory path
  too. The injected `&dyn Fn()` design keeps this testable.

### Step 6 -- severity-aware render (`cli/src/status.rs`, `cli/src/tui/view/mod.rs`)

- Add the `AlertCause::EnospcRisk { .. } =>` arm to the exhaustive cause match in the
  status renderer (around the `BtrfsDeviceErrors`/`MissingDevice`/`SmartdAlert`/
  `ComputationError` arms). ASCII line, e.g.
  `"  - ENOSPC risk: pool is one disk-loss from being unable to restore RAID1 redundancy"`.
  JSON (`StatusReport.alert_causes`) serializes automatically. The compiler forces
  this arm, so it cannot be forgotten.
- **Severity-aware banner (F3).** Both alert banners are hardcoded today to the
  critical line `"ALERT -- disk health issue detected. ..."` for *any* active alert
  (`status.rs#format_status_human`; `cli/src/tui/view/mod.rs#view`), which would
  misrepresent a non-beeping `Warning` as a dying disk -- the exact mis-signal
  decision #1 (severity-tiered notification) exists to prevent. Make both severity-
  aware by branching on the active alert's max severity (compute via
  `AlertCause::severity()` over `report.alert_causes` in status; the TUI already holds
  the full `AlertState`, so use `alert_state.severity()`):
  - `Critical` -> the existing red `"ALERT -- disk health issue detected. Run 'braid
    ack' ..."` text/style (unchanged).
  - `Warning` -> a distinct, lower-urgency ASCII banner (e.g. `"NOTICE -- capacity
    risk detected. Run 'braid ack' to acknowledge."`) and, in the TUI, a non-red
    (amber/yellow) style. Mixed Warning+Critical renders `Critical` (max wins).
  - The `severity()` `None` arm (empty `AlertState`, unreachable when a banner is shown
    since rendering is gated on `alert_active`/`active()`) is **fail-closed**: render
    the `Critical` banner, never drop to no banner.
  Keep the per-cause loop in `format_status_human` (it now includes the `EnospcRisk`
  line). The TUI banner is a single line (it does not enumerate causes), so only its
  text + style branch on severity.
- `build_status` already appends the live `enospc_risk_advisory` text; keep it (it is
  the detailed remediation). The latched cause shows in the alert section -- minor,
  acceptable overlap; optionally suppress the advisory line when the cause is latched.

Every new `pub`/top-level item gets a `///` explaining *why* it exists (project
convention), and all new user-facing strings stay ASCII.

## Tests

**Capacity** (`cli/src/capacity.rs#tests`): `evaluate_enospc_risk` typed result for
single-disk -> not-applicable, degraded -> not-applicable, healthy large/tiny,
at-risk 2-disk-one-low, at-risk 3-disk-loss-sim, survivor-threshold correctness, and
`margin` sign/monotonicity (`margin < 0` iff at-risk; worse pool -> more-negative
margin). **Re-arm-surplus regression (F2):** the existing healthy
`enospc_risk_advisory_silent_on_4_disk_with_one_low` shape (one device at 50 MiB)
returns a *large positive* margin -- pin that, so a predicate-healthy pool with one low
device re-arms rather than being stuck below a min-headroom gate. Keep a regression
test that `enospc_risk_advisory` still emits the exact legacy string (the extraction
must be behavior-preserving). Pin `ENOSPC_WORSEN_STEP`/`ENOSPC_REARM_MARGIN`.

**Alert** (`cli/src/alert.rs#tests`): `same_cause_key` treats `EnospcRisk` as a
singleton (replace, not duplicate); `merge_into_latch` carries a latched `EnospcRisk`
forward when absent this cycle (latched-until-ack); `AlertSeverity` max
(`Critical` wins over `Warning`; warning-only -> `Warning`); serde round-trip of
`EnospcRisk` through `save/load_alert_latch`; `EnospcAck` save/load/remove round-trip.

**Monitor** (`cli/src/monitor.rs#tests` + Step 0 fixtures):
- enter risk -> `MonitorResult::Alert` with exactly `EnospcRisk`, latch round-trips,
  and `state.severity() == Warning`.
- **probe-failure isolation** (key test): inject `UsageResult(Err(..))` alongside a
  device-error stats payload -> the `BtrfsDeviceErrors` cause is still latched, **no**
  `EnospcRisk`, and **no** `ComputationError` folded from the usage failure.
- usage-probe failure alone on an otherwise-healthy pool -> `MonitorResult::Ok` (no
  spurious alert), stderr logged, and a matching-key `enospc-ack.json` seeded
  beforehand still exists afterward -- the other skip-without-evaluating path also
  leaves the baseline untouched. Lower-risk than the degraded case (this path never
  reaches the re-arm branch), but cheap to pin.
- suppressed-while-acked: matching-key baseline + still at-risk (not worse) -> no
  fresh `EnospcRisk`.
- re-fire-when-worse: matching-key baseline + `margin` past the step -> `EnospcRisk`.
- re-arm-after-clear: baseline present + pool healthy by the predicate margin (use the
  4-disk-one-low *healthy* usage so this also guards F2 at the monitor level: a low
  device must not block re-arm) -> baseline removed, no cause; a follow-up at-risk
  cycle fires again.
- **stale-baseline / key mismatch (F1):** baseline whose `pool_key` differs from the
  live pool + at-risk usage -> `EnospcRisk` fires and the stale `enospc-ack.json` is
  removed (not suppressed). Cover all three mismatch axes in separate cases: changed
  devid set, changed FS UUID, and -- the gap this round closes -- **same devid, changed
  `device_size`** (a `braid replace`/resize), where `fs_uuid + devids` alone would
  still match.
- **identity gap / missing `fs_uuid` (F2):** live key `None` (probe yields no
  `fs_uuid`) + at-risk usage + a present baseline -> `EnospcRisk` fires (armed) and the
  baseline file is **left in place** (not removed -- distinguishes the can't-compare
  case from a confirmed mismatch).
- **corrupt baseline + live risk (F3):** `enospc-ack.json` is unreadable/unparseable +
  at-risk usage -> `EnospcRisk` fires (armed), `ComputationError` is **not** folded,
  and the corrupt file is cleared best-effort. (Pins the "risk-known, baseline-lost"
  branch as distinct from a usage-probe failure.)
- degraded (`missing_count > 0`) -> no `EnospcRisk` even with tight usage **and** a
  matching-key `enospc-ack.json` seeded before the cycle survives it (assert the file
  still exists). This pins the design's "leave any baseline untouched" guarantee --
  skip *before* the state machine: a sentinel-reliant impl that let the degraded
  `i64::MAX` margin reach the `margin >= ENOSPC_REARM_MARGIN` re-arm branch would call
  `remove_enospc_ack` and silently drop the baseline on every degraded cycle, losing a
  still-at-risk pool's suppression memory across a transient device absence (same
  devid + `device_size` on reconnect, so the key still matches and *should* suppress).
  The bare "no cause fires" assertion stays green under that bug, because the sentinel
  also suppresses the cause -- so the file-survival assertion is what makes the test
  fail if the guarantee is reverted.

**Ack** (`cli/src/ack.rs#tests`): mounted ack of an `EnospcRisk` latch (usage probe
stubbed) writes `enospc-ack.json` whose `pool_key.devices` carry the `(devid,
device_size)` pairs from the ack-time usage probe and whose `baseline_margin` is the
*re-probed* current margin (not the latched fire-time value), and clears the latch but
leaves the baseline file in place; mounted ack when the usage probe fails / is
unstubbed (`MissingMock`) or yields no `fs_uuid` clears the latch but writes **no**
baseline (assert absent); offline ack of an `EnospcRisk`-only latch is allowed, clears
the latch, and writes **no** baseline (assert `enospc-ack.json` absent afterward); and
the injected cleanup hook **fires** during cleanup (unit scope only -- the test hook is
an injected `&dyn Fn()` that records invocation; it does not shell out, mirroring the
existing `braid-alert.service` stop coverage and `cleanup_alert_files_and_beeper`'s
"the invariant is that the hook is invoked, not that sound was proven stopped"). The
actual `systemctl stop braid-alert-advisory.service` (the extended `ack.rs#stop_beeper`
body) is verifiable only with real systemd, so it is asserted in the VM e2e, not here.

**Render -- severity-aware banners (F3)** (`cli/src/status.rs#tests`,
`cli/src/tui/view/mod.rs` tests): exact-output assertions that a `Warning`-only
(`EnospcRisk`) active alert renders the lower-urgency banner -- **not** the critical
`"ALERT -- disk health issue detected"` text -- while a `Critical` cause
(`BtrfsDeviceErrors`/`MissingDevice`/etc.) still renders the existing critical banner,
and a mixed Warning+Critical state renders `Critical`. For the human status, assert the
exact banner string for each; for the TUI, assert the rendered banner line **text**
(e.g. `"NOTICE -- capacity..."` vs `"ALERT -- disk health..."`) via the existing
`cli/src/tui/test_support.rs#buffer_to_string` / `snap!` harness -- this is the
behavioral, structure-insensitive signal. Note `buffer_to_string` deliberately drops
styles (text only, per its doc-comment), so the *non-red color* is **not** assertable
through it; treat the warning banner's color as out of scope (presentation, below the
project's test bar). If the color is ever worth pinning, assert it by reaching into the
raw `TestBackend` buffer cells' style directly, not via `buffer_to_string`.

**VM e2e** (new `tests/cli/braid-monitor-enospc.{nix,py}`, modeled on
`tests/cli/braid-monitor.py` + `tests/cli/braid-remove-enospc.py`): build a RAID1
pool, fill it so per-device unallocated drops below threshold, assert `braid monitor`
exits 3 (not 1, no beep), `braid status --json` `alert_causes` contains
`{"type":"enospc_risk"}` and the human status shows the ENOSPC line, the configured
`alertCommand` ran, `braid ack` clears it **and stops `braid-alert-advisory.service`**
(assert the unit is no longer active -- the real-systemd check the unit tests cannot
make), a follow-up `braid monitor` exits 0 (re-arm); a degraded pool does **not** raise
it; and -- guarding F1 end-to-end --
after acking an at-risk pool, a `braid add` (topology change) invalidates the prior
baseline so a still-at-risk state re-fires rather than staying suppressed. Each test
opens with the
`// Intent / Why it exists / Scenario` preamble (per `docs/dev/testing.md`) and is
registered in `flake.nix` `checks`.

## Docs / ADRs to update

- **ADR 014** (`docs/design/decisions/014-alerts.md`): add `EnospcRisk` to the cause
  list; introduce `AlertSeverity` (Warning vs Critical) and the beep-reserved-for-
  Critical rule; document the level/baseline ack semantics (monotonic risk-magnitude,
  re-arm on clear) and the dedicated `enospc-ack.json` file; document that the baseline
  is bound to a `pool_key` (FS UUID + sorted per-device `(devid, device_size)`) and
  invalidated on bootstrap/membership/geometry change -- including a same-devid
  `braid replace`/resize, which `device_size` (not bare devid) catches -- the
  `EnospcRisk` analog of the `#acked-stats-hygiene-across-pool-membership-changes`
  guarantee; note it reuses the `MissingDevice` self-re-arm precedent so the
  sticky-latch invariant holds. **Crucially, 014's own `#braid-monitor-is-a-pure-detector`
  section restates the exit-code/beeper contract in prose ("the systemd wrapper starts
  the beeper on exit 1", "Exit 2 means the monitor never ran") -- this is now stale and
  must be revised to the severity model: exit 1 = Critical (wrapper beeps), exit 3 =
  Warning-only (wrapper notifies via `alertCommand`, no beep), exit 2 = never ran. To
  avoid the dual-maintenance drift the reviewer flagged (the exit-code contract is
  duplicated across two Active ADRs), make **ADR 018 the single owner of the exit-code
  enumeration** and have 014 reference it rather than re-list the numbers -- 014 keeps
  the severity->beep *semantics* (its domain), 018 keeps the exit-code->wrapper *table*
  (its domain; AGENTS.md already routes "the wrapper" to 018, and 018 already
  cross-references 014 for the cause taxonomy). The 014->018 cross-reference must be a
  valid mdBook link and, if placed in a `## See` section, satisfy
  `scripts/docs/check-see-paths.py` (doc-citations.md#decision-doc-references).
  By the **same** ownership split, 014's pure-detector section holds one more
  now-stale absolute: its fail-closed sentence ("any failure inside `cmd_monitor`
  that leaves pool state indeterminate latches a `ComputationError` cause and reports
  exit 1") admits **no** exception today, but after this change the best-effort
  `btrfs device usage` probe is exactly one sub-probe whose failure skips only
  `EnospcRisk` and deliberately does **not** latch `ComputationError`. Because 014 is
  the canonical `ComputationError`/cause-taxonomy authority that 018 already defers to
  (018's exit-code section links here "for the cause taxonomy"), name this carve-out
  in 014's `#braid-monitor-is-a-pure-detector` fail-closed section -- the best-effort
  ENOSPC usage probe skips only `EnospcRisk` and never latches `ComputationError`,
  cross-referencing 018 for the probe *mechanism* -- so the authority doc names its
  own sole exception instead of reading as an absolute a maintainer could "fix." Leave
  018's mechanism-side fail-open description (below) as written, and verify the
  carve-out by reading both ADRs' fail-closed prose, since `just docs-build` only
  link-checks.
- **ADR 018** (`docs/design/decisions/018-systemd-lifecycle.md`): this is the
  **canonical exit-code table**. Update the `braid-monitor` health-polling section's
  exit-code reservation (today it enumerates 0/1/2) to add exit code **3** (warning-only
  alert active, non-beeping) and the wrapper routing for it; state the beep is reserved
  for exit 1 / Critical. Document the best-effort usage probe and the **scoped fail-open
  exception** to the fail-closed mandate (probe-evaluation failure skips only
  `EnospcRisk`, never latches `ComputationError`). `just docs-build` only link-checks,
  so verify both ADRs' exit-code prose by reading, not just CI.
- `docs/commands/monitor.md` ("Does NOT probe ENOSPC" -> now does, at Warning
  severity, no beep), `docs/commands/status.md` / `docs/commands/doctor.md` (note the
  shared `evaluate_enospc_risk` predicate), and `modules/braid/monitor.nix`
  `alertCommand` option description (now also fires on ENOSPC warnings).
- Review `docs/design/principles.md` for any monitor-scope/alert-cause invariant that
  must reflect the new severity axis; sync `README.md` if it documents monitor/alert
  behavior.

## Verification (end-to-end)

1. `just test-rust` -- all new and existing unit tests (Steps 0-6).
2. `just test-parsers` -- confirm the `btrfs device usage --raw` fixtures parse.
3. Build the new VM check (`nix build .#checks.aarch64-darwin.braid-monitor-enospc`
   via the linux-builder) -- the full enter -> exit-3 -> alertCommand -> ack ->
   re-arm lifecycle and the degraded no-false-positive case.
4. `just docs-build` -- mdBook link-check for the ADR/command-doc edits.
5. Spot-check ASCII: `scripts/docs/check-output-ascii.py` over the new `cli/src`
   strings and the `monitor.nix` echo/script lines.

## Deferred / open questions

- **Metadata-pressure sibling cause.** The agreed end-state covers metadata ENOSPC too,
  but as a *separate* `MetadataEnospcRisk` cause (it needs a second `btrfs filesystem
  df` probe and has the opposite remediation -- never balance metadata). It follows the
  exact pattern landed here (typed predicate extracted from
  `doctor.rs#check_metadata_enospc_pressure`, Warning severity, its own baseline) and is
  scoped as the fast-follow, not part of this change.
- **Status display overlap.** Whether to suppress the live `enospc_risk_advisory` line
  when the latched `EnospcRisk` cause is already rendered is a cosmetic call to settle
  during implementation; both convey the same condition.
- **Durable offline suppression.** Offline ack intentionally writes no baseline (it
  cannot probe the live `pool_key`), so a still-at-risk pool re-fires once at the quiet
  warning level on remount. If that proves annoying, a follow-up can derive the
  `pool_key` from `pool.json` membership so offline ack can write a keyed baseline
  without a live probe. Out of scope here.
- **Plain-latch fallback seam (not planned; documented escape hatch).** The keyed
  monotonic baseline (`PoolKey` + `live_pool_key` + `device_size` keying + tri-state
  load + the three monitor failure-mode branches + the ack re-probe + `WORSEN_STEP`/
  `REARM_MARGIN` + ~half the new monitor/ack tests) is the single largest subsystem in
  this change. Decision #2 (locked with the user) keeps it, because it buys the *fresh*
  nudge when an acked-but-still-filling pool gets materially worse before it clears. If
  implementation risk or timeline pressure appears, the clean fallback is to ship
  `EnospcRisk` as a plain latch-until-ack Warning (exactly like `SmartdAlert`): delete
  the entire baseline subsystem, keep the cause + severity tier + exit-3 wiring + status
  render, and still deliver the core value (proactive, non-beeping capacity alerts),
  with the keyed baseline as a documented fast-follow -- the same staging already used
  for the metadata-pressure sibling. This is recorded only as a seam; the plan as
  written implements the keyed baseline.
- **`ENOSPC_WORSEN_STEP` / `ENOSPC_REARM_MARGIN` exact values** are pinned by tests
  during Step 1 (starting point: one btrfs data chunk, ~1 GiB).

## Implementation notes

- **`ENOSPC_WORSEN_STEP` is half a chunk (512 MiB), not the plan's ~1 GiB.** An
  at-risk `margin` is bounded in `[-threshold, 0)` (unallocated and chunk-pair
  capacity are both >= 0, threshold caps at 1 GiB), so a full-chunk worsen step
  makes the re-fire branch `margin < baseline_margin - WORSEN_STEP`
  mathematically unreachable (both terms are >= -1 GiB). The plan delegated the
  exact values to implementation ("pinned by tests"); `GIB / 2` keeps re-fire
  reachable while staying meaningful. `ENOSPC_REARM_MARGIN` stays 1 GiB (positive
  margins are unbounded). Pinned by `capacity::tests::enospc_hysteresis_constants_pinned`.
- **`load_enospc_ack` reuses `LatchLoadError`** (the existing Read/Parse tri-state)
  rather than introducing a near-duplicate error type -- the monitor already
  handles that shape.
- **`MonitorTestRunner` gained an always-present `usage_payload` field** (default
  healthy) alongside the single override slot, so the "at-risk usage + custom
  show" tests (identity-gap, degraded) set usage via the field and the show via
  the slot without contention. The plan's `with_stats_payload_and_usage` is kept
  for the probe-failure-isolation precondition (custom stats + `UsageResult(Err)`).
- **`live_pool_key` lives in `alert.rs`** next to `PoolKey` (it builds a `PoolKey`
  from `BtrfsDeviceUsageEntry`, a parse type both monitor and ack already import),
  rather than in `probe.rs` where the plan listed it.
- **Status display overlap left in place** (the deferred cosmetic question):
  `build_status` still appends the live `enospc_risk_advisory` text even when the
  latched `EnospcRisk` cause renders in the alert section. Both convey the same
  condition; the plan marked suppression as optional.
