# Add first-detected timestamps to braid alert latches

## Context

`braid monitor` latches alert causes into `alert-latch.json`, and they stay
latched until `braid ack` (ADR 014's sticky-latch invariant). Today the latch
records only *what* (the `AlertCause`), never *when*. So `braid status` shows an
alert with no hint that it may be a historical incident the live pool has since
recovered from -- the operator can't tell a latch that fired 5 minutes ago from
one that fired last week.

This change records, per cause, **when the monitor first latched it**, and shows
that time in `braid status`. The point is to make explicit that latched alerts
are *incidents* ("this happened, ack it"), not necessarily live facts.

**Scope:** timestamps only. We do NOT add "currently active vs resolved" status.
The timestamp enriches the latch; it does not change when alerts appear or clear.
The sticky-latch invariant (alerts persist until `braid ack`) is preserved
verbatim.

**No backwards compatibility:** there are no existing `alert-latch.json` files in
the wild (single user, no live latches). The on-disk `detected_at` is a required
field -- a latch entry without one is malformed and should fail to parse (matching
braid's fail-loud latch philosophy in `status.rs#resolve_alert_state`).

### Locked design decisions

- **Time format:** RFC3339 UTC seconds string everywhere -- on disk, in
  `status --json`, and in human output. `detected_at` is display-only: the merge
  path only carries it forward (string clone), nothing compares or computes on it,
  so storing the displayed form is correct and avoids a conversion layer.
  (`EnospcAck::snoozed_until` is a `u64` *because the monitor integer-compares it*;
  that rationale does not apply here.) RFC3339-UTC-seconds still sorts
  chronologically as a plain string if ordering is ever wanted.
- **Human render:** per-cause, inline, absolute timestamp + relative age, e.g.
  `  - missing device: disk2 (devid 2) -- first detected 2026-06-25T15:35:54Z (2 hours ago)`.
  Per-cause (not a banner summary) because causes latch at different times.

## Design overview

Two layers, two types -- mirroring braid's existing split between internal state
and the `StatusReport` DTO:

1. **Latch layer** (`alert.rs`, written by monitor, read by status/ack): a new
   wrapper `LatchedCause { detected_at: String, cause: AlertCause }` with a
   **required** RFC3339 timestamp. `AlertState.causes` becomes `Vec<LatchedCause>`.
   `AlertCause` itself stays a pure "what" descriptor (unchanged) -- live causes
   from `compute_alert_state` carry no time; the timestamp is a property of being
   *latched*.

2. **Status-view layer** (`status.rs`, the `--json` + human surface): a new
   `AlertCauseReport { cause: AlertCause, first_detected: Option<String> }`. The
   timestamp is **optional** here because `resolve_alert_state` synthesizes
   transient "bridge" causes (a smartd/scrub flag observed before the next monitor
   cycle latches it; the cleanup-pending sentinel; an unreadable-latch
   `ComputationError`) that have no persisted detection time. Latch-derived causes
   carry `Some(ts)`; bridge causes carry `None` and render with no timestamp.

3. **Shared time helpers** (`util.rs`): promote/centralize the RFC3339 formatter,
   add a parser and a single relative-age humanizer reused by both `status` and the
   TUI.

## Changes

### Shared time helpers -- `cli/src/util.rs`

`util.rs` already owns time/format helpers (`util.rs#now_iso`,
`util.rs#format_duration_secs`). Add three siblings:

- `format_rfc3339_utc_seconds(now: SystemTime) -> String` -- **move** the existing
  private `membership.rs#format_rfc3339_utc_seconds` here and make it
  `pub(crate)`; update `membership.rs` to call the shared one. Used by the merge
  stamp and the status absolute render.
- `parse_rfc3339_utc(s: &str) -> Option<OffsetDateTime>` -- parse a stored
  timestamp back for age computation (`time::OffsetDateTime::parse` with the
  well-known `Rfc3339`; our seconds-only output is valid RFC3339). `Option` so the
  renderer degrades gracefully on a malformed value.
- `humanize_ago(diff: time::Duration) -> Option<String>` -- the single relative-age
  humanizer. `None` for a negative/future diff (clock skew). Buckets: `<1 min ago`,
  `N min ago`, `1 hour ago` / `N hours ago`, `1 day ago` / `N days ago`. This is a
  **promote-and-improve** of `tui/view/mod.rs#timeago`, which today is TUI-local,
  takes a `PrimitiveDateTime`, and has **no hours bucket** (5h reads as
  "300 min ago"). Decoupling it to operate on a `time::Duration` lets both the UTC
  status path and the naive-local TUI path reuse it; adding the hours bucket also
  fixes the TUI scrub "last run" rendering.

### Latch schema -- `cli/src/alert.rs`

- New `LatchedCause`:
  ```rust
  #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
  pub struct LatchedCause {
      /// RFC3339 UTC seconds when the monitor FIRST latched this cause.
      /// Preserved across refreshes (same `same_cause_key`); only a fresh
      /// append stamps it. Display-only -- never compared or parsed back here.
      pub detected_at: String,
      #[serde(flatten)]
      pub cause: AlertCause,
  }
  ```
  Provide a terse constructor (`LatchedCause::new(cause, detected_at)`) to keep
  fixtures readable.
- `AlertState { pub causes: Vec<LatchedCause> }`. `AlertState#active` unchanged;
  `AlertState#severity` becomes `self.causes.iter().map(|c| c.cause.severity()).max()`.
- `compute_alert_state` -> return `Vec<AlertCause>` (live, untimestamped) instead of
  `AlertState`. Update the one production caller (`monitor.rs#cmd_monitor`) **and
  every test call site** -- the `probe.rs` test (today reads
  `alert.causes.contains(...)`) and the ~16 `alert.rs` tests that read
  `.active()`/`.causes` on the returned value -- to consume the returned
  `Vec<AlertCause>` directly. The compiler forces all of these; the enumeration is
  for scope accuracy, not because any are subtle.
- `merge_into_latch(existing: Option<&AlertState>, live_causes: &[AlertCause], now: SystemTime) -> AlertState`:
  - carry forward `Vec<LatchedCause>` from `existing`;
  - for each live cause, match by `same_cause_key(&entry.cause, new_cause)`
    (`same_cause_key` stays operating on `AlertCause`, unchanged);
  - **refresh** (key match): replace `entry.cause = new_cause.clone()`, **keep
    `entry.detected_at`** -- this is the first-detection guarantee;
  - **append** (new): push `LatchedCause::new(new_cause.clone(), format_rfc3339_utc_seconds(now))`.
- `load_alert_latch` / `save_alert_latch` / quarantine: unchanged in shape;
  they round-trip the new `AlertState` automatically.

### Monitor -- `cli/src/monitor.rs`

- Introduce `cmd_monitor_at(runner, fs, mount_point, paths, now: SystemTime)` and
  make `cmd_monitor(...) = cmd_monitor_at(..., SystemTime::now())` (the established
  `_at` injection convention, e.g. `membership.rs`). Use the single injected `now`
  for **both** the existing `evaluate_enospc_for_monitor` call and the new
  `merge_into_latch(existing_latch.as_ref(), &live_causes, now)` stamp. `main.rs`
  keeps calling `cmd_monitor`; tests call `cmd_monitor_at` with fixed clocks.
- `live_causes` becomes `compute_alert_state(...)` (now a `Vec<AlertCause>`).

### Status -- `cli/src/status.rs`

- New view type:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  pub struct AlertCauseReport {
      #[serde(flatten)]
      pub cause: AlertCause,
      #[serde(default, skip_serializing_if = "Option::is_none")]
      pub first_detected: Option<String>,
  }
  ```
- `StatusReport.alert_causes: Vec<AlertCauseReport>` (was `Vec<AlertCause>`). JSON
  stays additive: each cause object keeps its `type`/`devid`/... and gains
  `first_detected` when known (the `skip_serializing_if` keeps bridge causes
  clean). Existing consumers that read `c["type"]` are unaffected.
- `resolve_alert_state` -> return `Vec<AlertCauseReport>` (its only callers are the
  two `StatusReport` builders and `tui/probe.rs#...`). Map each latched
  `LatchedCause` to `first_detected: Some(detected_at)`; the synthesized bridge
  causes (smartd/scrub/cleanup-pending/unreadable-latch) to `first_detected: None`.
  Callers derive `alert_active = !causes.is_empty()` and severity from
  `c.cause.severity()` as before.
- `format_status_human(report, ..., now: SystemTime)` -- thread a `now` param
  (cmd_status passes `SystemTime::now()`; tests pass a fixed value). For each cause
  line, when `first_detected` is `Some(ts)`, append ` -- first detected {ts}`, and
  if `parse_rfc3339_utc(ts)` succeeds and `humanize_ago(now - parsed)` is
  `Some(age)`, also append ` ({age})`. Degrade to absolute-only on parse failure or
  a future timestamp. Bridge causes (`None`) render exactly as today.

### Ack -- `cli/src/ack.rs`

`cmd_ack` reads the latch to decide which flag files to clear; update those match
sites to read `c.cause` instead of `c`. The lifecycle is otherwise untouched: ack
still `remove_alert_latch`es -- the sticky invariant (clear only on ack) is
unchanged.

### TUI -- `cli/src/tui/`

Update for the type change so it compiles (`resolve_alert_state` now yields
`AlertCauseReport`; alert views read `c.cause`). Rewire
`tui/view/mod.rs#timeago` to delegate to the shared `util::humanize_ago`. Rendering
`first_detected` in the TUI alert view is a natural consistency win but is
secondary to the `braid status` scope; include it if cheap.

### Docs

- **ADR 014** (`docs/design/decisions/014-alerts.md`, stays `Active`): two updates.
  - **Rewrite the architecture wording for the type split.** The current
    `### Shared alert computation` section says "a single shared computation
    produces an `AlertState` consumed by all surfaces" -- which the split now
    contradicts. Name the three layers explicitly: live detection produces
    `Vec<AlertCause>`; the persisted latch is `AlertState { causes: Vec<LatchedCause> }`
    (each entry stamped with `detected_at`); `braid status` / TUI consume
    `AlertCauseReport` (cause + optional `first_detected`). Also reconcile the
    `### Latched alerts` / `### Latch as append/refresh log` and
    `### Corrupt latch recovery` sections, which still describe `AlertState`/
    `load_alert_latch` purely in terms of causes.
  - **Document the first-detected timestamp:** semantics ("when the monitor first
    latched this cause", preserved across refreshes), that it enriches the latch
    without changing when alerts appear/clear, the RFC3339 storage choice, and that
    status-synthesized bridge causes legitimately carry no timestamp.
- **`docs/commands/status.md`**: update the `alert_causes` JSON schema to include
  `first_detected`, and the human output example to show the
  `first detected ... (N ago)` line. (The `alert.rs` doc-coverage test that every
  `AlertCause` variant is documented still passes -- no new variants -- but the
  schema text must be accurate.)
- **`README.md`**: sync any shown `braid status` alert output.

## Tests

Follow the `// Intent / Why it exists / Scenario` preamble
(`docs/dev/testing.md`).

**Rust unit -- `cli/src/util.rs`:**
- `humanize_ago` bucket boundaries with fixed `Duration`s: 59s -> `<1 min ago`,
  60s -> `1 min ago`, 59m -> `59 min ago`, 60m -> `1 hour ago`, 23h -> `23 hours ago`,
  24h -> `1 day ago`, 48h -> `2 days ago`; negative diff -> `None`.

**Rust unit -- `cli/src/tui/view/mod.rs`:**
- Add an hours-range case to `timeago_buckets_and_future_none` (e.g. a 5-hour-old
  `PrimitiveDateTime` -> `"5 hours ago"`). Today that test jumps from a 30-min case
  straight to a 1-day case with no 1h-23h coverage, so the advertised hours-bucket
  fix would silently survive a revert to the old no-hours buckets. This case also
  pins the `timeago` -> `util::humanize_ago` delegation at the surface it fixes.

**Rust unit -- `cli/src/alert.rs`:**
- `merge_into_latch` stamps `detected_at` on first append: fixed
  `now = UNIX_EPOCH + 1_700_000_000s` -> entry `detected_at == "2023-11-14T22:13:20Z"`.
- **`merge_into_latch` preserves `detected_at` on refresh** (the core guarantee):
  append at `t0`, re-detect the same key at `t1 > t0`, assert `detected_at` stays
  `t0`'s string while `cause` reflects fresher evidence.
- `AlertState` save -> load round-trips `detected_at` exactly, and a serialized
  `LatchedCause` has top-level `"type"` **and** `"detected_at"` keys with **no
  nested `"cause"` object** -- pins the flat public shape regardless of whether
  it is achieved via `#[serde(flatten)]` or a manual impl (see Risks).
- **Migrate the three existing exact-bytes JSON-shape tests**, which break twice
  over: they build `AlertState { causes: vec![AlertCause::...] }` (no longer
  compiles), and their `{"type":...}` substrings -- with `{` immediately before
  `"type"` -- vanish once `detected_at` serializes first (declaration order). The
  three are `alert_state_json_shape_bare_integer_devid`,
  `enospc_risk_latch_roundtrip_and_json_shape`, and
  `scrub_failed_latch_roundtrip_and_json_shape`. Rebuild each over `LatchedCause`
  and update the substring to the wrapper shape (`{"detected_at":"<ts>","type":...}`)
  while **carrying forward what each uniquely pins**: the bare-integer `devid`
  contract (`"devid":7`, not a wrapped form -- the `bare_integer_devid` preamble
  exists precisely to catch a serde change "that switches to a wrapped form", i.e.
  this change) and the exact `enospc_risk` / `scrub_failed` field sets. Assert both
  `"detected_at":"..."` and the cause fields are present, with **no** `"cause"` key.
  The generic round-trip test above does not re-pin the bare-integer `devid` or the
  enospc field set, so a "make it compile / loosen the substring" fix must not
  silently drop that coverage.
- **Revise `load_alert_latch_accepts_legacy_active_key`**: its fixture
  (`{"active":true,"causes":[{"type":"missing_device","devid":7}]}`) feeds
  cause-only objects with no `detected_at`, which must no longer parse. Update it to
  timestamped causes (`{...,"causes":[{"detected_at":"2023-11-14T22:13:20Z","type":"missing_device","devid":7}]}`)
  so it keeps asserting that the legacy top-level `"active"` key is ignored and
  `active` is derived from the (now timestamped) causes. **Also rewrite its `//`
  preamble:** today it frames the test as guarding cross-version load of
  "pre-refactor latches written by an older binary," which the new
  required-`detected_at` policy directly contradicts -- a genuine prior-version
  cause-only latch must now *fail* to parse (that is exactly the new negative test
  below). Re-scope the preamble to what the test still verifies: serde ignores an
  unknown top-level `"active"` key (no `deny_unknown_fields` on `AlertState`) and
  `active()` derives from causes. Drop the old-binary-compatibility narrative.
- **New negative test** pinning the no-backcompat policy: a latch whose cause object
  is missing `detected_at` makes `load_alert_latch` return `LatchLoadError::Parse`
  (not silently accept a timeless cause). This is what makes "old cause-only entries
  are malformed" executable rather than aspirational.

**Rust unit -- `cli/src/status.rs`:**
- `format_status_human` with a fixed `now` and a latched cause renders the exact
  `-- first detected <ts> (<age>)` suffix.
- **Render-time degradation** (`Some(ts)` present but age uncomputable ->
  absolute-only): a `format_status_human` case with a fixed `now` and a latched cause
  whose `first_detected` is a **future** timestamp renders `-- first detected <ts>`
  with **no** `(... ago)` suffix. This pins the middle branch between the happy path
  (age present) and the bridge `None` path (no timestamp at all) -- otherwise only
  `humanize_ago`'s negative-diff `None` is pinned, at the helper level, not at the
  render surface. A malformed-but-string `first_detected` (which `parse_rfc3339_utc`
  rejects -- the latch stores `detected_at` as an opaque `String`, so `load_alert_latch`
  accepts a non-RFC3339 value and only the renderer parses it back) hits the same
  no-age branch; add it too for belt-and-suspenders coverage of both sub-paths. This
  guards against a later regression into an `unwrap` (crashing `braid status` on a
  weird latch) or a lost absolute fallback -- both of which violate braid's
  fail-loud-don't-crash latch philosophy.
- A bridge cause (`first_detected: None`) renders with **no** timestamp suffix
  (extend the existing `resolve_alert_state_*_smartd*` / cleanup-pending tests).
- `StatusReport` JSON: a latched cause serializes `first_detected`; a bridge cause
  omits it; round-trips.

**Rust integration -- `cli/src/monitor.rs`:**
- `cmd_monitor_at` end-to-end: cycle 1 at `t0` latches a new cause stamped `t0`;
  cycle 2 at `t1` re-detecting the same cause leaves `detected_at == t0` on disk
  (sticky first-detection across cycles). Update the existing
  `unmounted_pool_preserves_existing_alert_latch` fixture to the timestamped shape;
  its byte-for-byte offline-survival assertion still holds.

**NixOS VM -- extend `tests/cli/braid-monitor.py`** (no new `flake.nix` check; we
extend an existing test). After the latch is created:
- `alert-latch.json` cause has a `detected_at` matching an RFC3339 regex;
- `braid status --json` cause object has `first_detected` matching the RFC3339 regex;
- `braid status` human output shows `first detected ` + the RFC3339 substring + a
  `(... ago)` suffix (regex, not exact -- wall-clock).
Assert **presence/format**, not exact values; exact timestamp/age semantics are
pinned in the Rust tests above. Existing survive-offline / clears-on-ack assertions
stay green.

**Hand-authored Python latch-fixture sweep (required-field migration).** Making
`detected_at` required means every *parseable* `alert-latch.json` a Python test
hand-writes must gain a `detected_at` per cause, or that test silently flips onto
the corrupt-latch path and stops exercising its intended behavior. Two such
fixtures exist today (the corrupt-on-purpose fixtures that write `'not json'` are
unaffected -- they already fail to parse):
- `tests/cli/braid-monitor.py` "Offline ack refused on mixed BtrfsDeviceErrors +
  MissingDevice latch" subtest hand-writes a two-cause `AlertState`
  (`{"causes":[{"type":"btrfs_device_errors","devid":1},{"type":"missing_device","devid":2}]}`)
  specifically to reach the refusal path rather than the corrupt path (see its
  inline comment). Add `detected_at` to each cause object. Its byte-for-byte
  `on_disk == latch_fixture` assertion still holds against the updated bytes.
- `tests/module/alert-state-lock.py#write_missing_latch` writes
  `{"causes":[{"type":"missing_device","devid":devid}]}`, consumed by the
  offline-ack and lock-contention subtests. Add `detected_at` so those fixtures stay
  parseable and keep testing the lock/offline-ack behavior (not the corrupt path).
Use a fixed literal (e.g. `"2023-11-14T22:13:20Z"`) -- these assert behavior, not
timestamp values.

## Verification

1. `just test-rust` -- new + updated unit/integration tests pass; in particular the
   refresh-preserves-`detected_at` and `humanize_ago` boundary tests.
2. `cargo build` clean (the `AlertState.causes` element-type change ripples through
   `monitor`, `status`, `ack`, `tui`, and many test fixtures -- mechanical).
3. Run the touched VM tests: the extended `braid-monitor`
   (`nix build .#checks.aarch64-darwin.braid-monitor`) **and** `alert-state-lock`
   (`nix build .#checks.aarch64-darwin.alert-state-lock`) -- the latter's
   `write_missing_latch` fixture is migrated to the timestamped shape, so it must be
   re-run to confirm it still reaches the lock/offline-ack paths rather than the
   corrupt-latch path.
4. Manual sanity in a VM/dev shell: trigger a degraded-pool alert, run
   `braid status` (human shows `first detected ... (N ago)`), `braid status --json`
   (cause has `first_detected`), run `braid monitor` again and confirm `detected_at`
   in `alert-latch.json` is unchanged, then `braid ack` and confirm the latch clears.
5. `scripts/docs/check-output-ascii.py` and `just docs-build` (linkcheck) pass.

## Risks / notes

- **`#[serde(flatten)]` over an internally-tagged enum** (`AlertCause` is
  `#[serde(tag = "type")]`) is used in both `LatchedCause` (disk) and
  `AlertCauseReport` (JSON), and both derive `Deserialize`. This combination
  generally works with `serde_json` but has historical edge cases. The round-trip
  unit test above is the gate -- write it first. **The flat shape is
  non-negotiable**: `{ "detected_at": ..., "type": ..., ... }` for the latch and
  `{ "type": ..., ..., "first_detected": ... }` for `status --json` is the public
  contract in `docs/commands/status.md#json-output` (consumers read `c["type"]`).
  **Fallback** if derive-based `flatten` misbehaves: write a manual
  `Serialize`/`Deserialize` (or a `#[serde(with = ...)]` adapter) that emits/accepts
  the **same flat shape**. Do **not** nest the cause under a `"cause"` key -- that
  would break `c["type"]`, the documented schema, and the existing python
  assertions.
- **Non-deterministic human output:** the `(N ago)` suffix changes over time, so VM
  assertions match format via regex, never exact age. Exact age is unit-tested with
  fixed clocks.
- **Render-time parse:** computing the age requires parsing the stored RFC3339 back
  to a time (the one place anything reads `detected_at` as more than a string).
  This is a render concern only; the latch merge never parses it, and the renderer
  degrades to absolute-only if the parse fails.

## Implementation notes

- `cmd_ack` consumes the latch by mapping `Vec<LatchedCause>` down to
  `Vec<AlertCause>` at its load boundary
  (`s.causes.into_iter().map(|c| c.cause).collect()` in `cli/src/ack.rs`), rather
  than threading `c.cause` through each downstream match site as the plan's
  wording suggested. The ack lifecycle only needs the "what" of each cause, never
  the timestamp, so dropping `detected_at` at the boundary keeps the rest of
  `cmd_ack` untouched.
- The TUI `PoolState` field became `alert_causes: Vec<AlertCauseReport>` (was
  `alert_state: AlertState`), following `resolve_alert_state`'s new return type so
  the TUI consumes the same status-view layer (`AlertCauseReport`) as `braid
  status` instead of re-deriving from the raw latch.
- The dedicated `format_rfc3339_utc_seconds` unit test moved from `membership.rs`
  to `util.rs` alongside the function it now lives next to.

## Follow Up

- The TUI alert widget (`cli/src/tui/view/mod.rs`) renders only a one-line
  severity banner, not per-cause detail lines, so `first_detected` is not
  surfaced in the TUI. The plan marked TUI rendering "include if cheap" -- it is
  not cheap here, since showing it would require adding a per-cause detail
  section to the alert widget.
