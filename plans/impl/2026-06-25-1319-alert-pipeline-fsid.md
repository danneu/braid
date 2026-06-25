# Adopt the `Fsid` newtype in the alert pipeline (retire bare-`String` `fs_uuid`)

## Context

The ENOSPC-risk snooze marker keys on the btrfs filesystem UUID. The codebase
already has a canonical newtype for that value -- `cli/src/types.rs#Fsid` --
whose own doc says it is "the single source of truth for 'which btrfs pool'"
and that "raw-string FSID comparison ... is the mix-up this type makes a compile
error." The btrfs-show parser already hands every consumer an `Option<Fsid>`
(`cli/src/parse/types.rs#BtrfsFilesystemShowOutput` field `uuid`).

The alert pipeline opts out of that type. `cli/src/probe.rs#probe_pool_alerts`
**downgrades** the parser's `Fsid` to a bare `String`
(`show.uuid.as_ref().map(|u| u.as_str().to_owned())`) and threads it as
`String`/`&str` through `AlertPoolState.fs_uuid`, `PoolKey.fs_uuid`,
`live_pool_key`, and the monitor/ack call sites -- exactly the raw-string FSID
handling `Fsid` exists to forbid. That fork is also why the field reads
ambiguously against ADR 024's "UUID is identity" vocabulary (which means the
**LUKS** UUID, a separate `LuksUuid` newtype): a reviewer cross-referencing
ADR 024 could mistake `PoolKey.fs_uuid` for the LUKS identity axis.

**Outcome:** one name and one type for the btrfs filesystem UUID across the whole
codebase (`fsid: Fsid`, matching `PoolState.fsid`, `probe_fsid`, and ADR 024's
"FSID" vocabulary). The downgrade disappears, the value is validated/canonicalized
at the boundary, and the LUKS-vs-FS confusion is dissolved at the type level
rather than papered over with a comment. This is the structural fix the
verify-issue pass landed on -- a pivot from the originally-proposed one-line doc
note.

Per the user's decision: **full rename** -- rename both the Rust field and the
on-disk JSON key `fs_uuid` -> `fsid`.

## Authority / invariants preserved

- ADR 014 (`docs/design/decisions/014-alerts.md`): the alert pipeline stays
  **devid-keyed** for per-device state; `PoolKey` remains a whole-pool geometry
  key (`fsid` + sorted `(devid, device_size)`); the same-devid-replace
  invalidation via `device_size` is untouched. The marker stays machine-local
  and disposable.
- ADR 024 (`docs/design/decisions/024-luks-uuid-identity.md`): `LuksUuid` remains
  the per-disk identity; this change does not introduce LUKS UUID into the alert
  path. Adopting `Fsid` aligns the alert path's vocabulary with the rest of the
  codebase.
- No new tri-state semantics: `Option<Fsid>` preserves the existing "FS UUID
  absent -> no usable `PoolKey`, fire armed but leave any stored marker in place"
  identity-gap behavior.

## Reused building blocks (do not reinvent)

- `cli/src/types.rs#Fsid` -- newtype with `parse(&str) -> Result<Self, _>`,
  `as_str()`, `Display`, derives `Clone, Debug, PartialEq, Eq, Hash, PartialOrd,
  Ord`, and **manual** `Serialize` (emits a plain string) / `Deserialize`
  (re-parses through `Fsid::parse`). Drop-in for every trait `PoolKey` derives.
- `cli/src/parse/types.rs#BtrfsFilesystemShowOutput` field `uuid: Option<Fsid>`
  -- the already-typed source; stop converting it away.
- House convention (from `lock.rs`, `preflight.rs`, `replace.rs`, `recover.rs`):
  pass `&Fsid` by shared reference; compare with derived `==`/`!=`; reach for
  `.as_str()` only at sysfs/argv/format sites; build test values with
  `Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()`.

## Production changes

All edits replace the field name `fs_uuid` with `fsid` and the type
`String`/`&str`/`Option<String>` with `Fsid`/`&Fsid`/`Option<Fsid>`.

1. **`cli/src/probe.rs`**
   - `AlertPoolState` field: `pub fs_uuid: Option<String>` ->
     `pub fsid: Option<Fsid>`. Rewrite the field doc: drop the "Carried as a
     plain `String` because the alert pipeline only compares it, never re-parses
     it" rationale (now false -- keeping `Fsid` requires zero re-parsing); state
     it carries the canonical pool FSID, `None` only when the probe could not
     extract it.
   - `probe_pool_alerts`: replace
     `let fs_uuid = show.uuid.as_ref().map(|u| u.as_str().to_owned());` with
     `let fsid = show.uuid;` (clean partial move -- later `show.devices` /
     `show.missing_devids` uses are disjoint fields). Update the unmounted-branch
     initializer (`fs_uuid: None` -> `fsid: None`) and the return struct
     (`fs_uuid` -> `fsid` field shorthand).
   - Fix the two now-inaccurate doc comments on `AlertPoolState` and
     `probe_pool_alerts` that claim the alert state omits "FSID": it now carries
     the FSID (as `Option<Fsid>`, tolerating absence -- the real distinction from
     `probe_pool`, which errors on a missing FSID).

2. **`cli/src/alert.rs`**
   - `PoolKey` field: `pub fs_uuid: String` -> `pub fsid: Fsid`. Keep the
     `device_size`-geometry rationale. Add one sentence folding in the original
     finding's intent: this is the btrfs **filesystem** UUID (pool identity),
     deliberately **not** the LUKS-UUID per-disk identity of ADR 024 -- the alert
     pipeline is devid-keyed per ADR 014's "Ack state keyed by btrfs devid".
   - `live_pool_key`: signature `fs_uuid: Option<&str>` -> `fsid: Option<&Fsid>`;
     body `let fsid = fsid?;` then construct `PoolKey { fsid: fsid.clone(), .. }`.
     Update its doc and the inline `fs_uuid + devids` comment to `fsid + devids`.

3. **`cli/src/ack.rs`** -- `write_enospc_baseline`: the call becomes
   `live_pool_key(pool.fsid.as_ref(), &entries)`. **Correction to note:** change
   `.as_deref()` -> `.as_ref()`. `.as_deref()` only worked because
   `String: Deref<Target=str>`; `Fsid` does not implement `Deref`, so
   `Option<Fsid>::as_ref()` is the way to get `Option<&Fsid>`. Update the two
   prose doc comments mentioning `fs_uuid`.

4. **`cli/src/monitor.rs`** -- `cmd_monitor` passes `pool.fsid.as_ref()` (was
   `.as_deref()`); `evaluate_enospc_for_monitor` parameter `fs_uuid: Option<&str>`
   -> `fsid: Option<&Fsid>`; its `live_pool_key(fsid, &entries)` call follows.

## On-disk format

`PoolKey`/`EnospcAck` derive `Serialize`/`Deserialize` with no `#[serde]`
attributes, so renaming the field renames the JSON key `fs_uuid` -> `fsid`.
`Fsid` serializes as the same plain string, so the value shape is unchanged.

A pre-upgrade `/var/lib/braid/enospc-ack.json` (key `fs_uuid`, or any malformed
fsid) fails to deserialize and routes through the **existing** corrupt-marker
path: `load_enospc_ack` returns `Err`, the monitor fires the ENOSPC reminder
armed (non-beeping Warning), removes the file best-effort, and does **not** fold
a `ComputationError`. This is the same graceful degradation ADR 014 already
documents for the margin-baseline removal (line ~109) and is exercised by
`cli/src/monitor.rs#cmd_monitor_corrupt_baseline_fires_armed_without_computation_error`.
No migration ships. Worst case on upgrade: one self-healing ENOSPC reminder for a
pool currently at risk with an active snooze.

## Docs changes

`docs/design/decisions/014-alerts.md#severity-tiers-and-the-enospc-baseline` is
the authority for marker identity and still uses the retired shorthand
`fs_uuid + devids` (the "Marker identity (`pool_key`)" paragraph, the lone `fs_uuid`
token in the whole docs tree). Update that shorthand to `fsid + devids` so the
authoritative doc matches the renamed field -- a **vocabulary-only** edit, no
invariant change. The descriptive "btrfs filesystem UUID" / "FS UUID" prose
stays accurate and is left as-is. Do **not** alter the
`### Severity tiers and the ENOSPC baseline` heading text: its slug is a stable
anchor linked from `018-systemd-lifecycle.md` and twice within `014-alerts.md`
itself, so changing it would break those cross-links. Those links are validated
by **mdbook-linkcheck2** during `mdbook build docs`, which is why editing this
Active ADR makes `just docs-build` a required verification step. (The
`scripts/docs/check-see-paths.py` guard is unrelated here: it scans only `## See`
sections and strips the `#anchor` before resolving a file path, so this
vocabulary-only body edit -- touching neither a `## See` section nor a path
citation -- does not exercise it.)

## Test / fixture changes

Existing **behavioral** tests are structure-insensitive and must keep passing
unchanged in intent (they guard the semantics the refactor must not alter):
`live_pool_key` requires an fsid and sorts devices; stale-key and same-devid
geometry mismatches fire and clear (`cmd_monitor_stale_baseline_key_mismatch_*`);
corrupt baseline fires armed without `ComputationError`; save/load/remove
round-trip; ack snooze/no-baseline paths.

Mechanical updates the type/rename forces:

- **Non-UUID literals must become valid `Fsid` inputs.** Three fixtures use
  strings that fail `uuid::Uuid::parse_str`: `"fs"` (two `EnospcAck` constructors
  in `alert.rs` snooze/window tests) and `"fs-uuid"` (the `live_pool_key` test).
  Replace with a canonical UUID, e.g. `Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()`.
  The `live_pool_key(Some(..))` call needs an `&Fsid`, so bind a `let fsid = Fsid::parse(..).unwrap();`
  and pass `Some(&fsid)`; `live_pool_key(None, ..)` is unchanged.
- **Construction sites:** every test/fixture building `AlertPoolState`/`PoolKey`
  switches `fs_uuid: "<uuid>".to_owned()` / `fs_uuid: None` to
  `fsid: Fsid::parse("<uuid>").unwrap()` / `fsid: None`. Sites already using
  valid UUIDs (the round-trip test's literal, the `*_FS_UUID` constants, the
  `"ffffffff-..."` mismatch literal) only change the wrapping, not the value.
- **Field-access assertions:** `result.fs_uuid.as_deref()` ->
  `result.fsid.as_ref().map(Fsid::as_str)` (or compare to
  `Some(Fsid::parse(..).unwrap())`); `ack.pool_key.fs_uuid` ->
  `ack.pool_key.fsid` compared via `.as_str()` against the `&str` constant.
- **JSON-key assertion:** the round-trip test's `on_disk["pool_key"]["fs_uuid"]`
  -> `on_disk["pool_key"]["fsid"]` (keep `.is_string()`).
- **Vocabulary cleanup (part of the full rename):** rename the test-fixture
  constants `ACK_FS_UUID` -> `ACK_FSID` (`cli/src/test_fixtures/ack.rs`) and
  `MONITOR_FS_UUID` -> `MONITOR_FSID` (`cli/src/test_fixtures/monitor.rs`),
  keeping them `&str` and parsing at use sites (mirrors `preflight.rs`'s
  `const FSID: &str = ...; Fsid::parse(FSID).unwrap()`). Rename test fns/comments
  embedding `fs_uuid` (e.g. `live_pool_key_requires_fs_uuid_and_sorts`,
  `cmd_ack_mounted_enospc_risk_no_fs_uuid_writes_no_baseline`). The probe.rs
  `result.fs_uuid` assertions are **not** in an `fs_uuid`-named test: they sit
  inside `probe_pool_alerts_mounted_2disk` (`result.fs_uuid.as_deref()`) and
  `probe_pool_alerts_tolerates_missing_fsid` (`result.fs_uuid, None`), whose names
  carry no `fs_uuid` token and stay -- only their in-body assertions change, per
  the "Field-access assertions" bullet above.

New tests to add. The "On-disk format" claim is that **both** a legacy-key
marker and a malformed-fsid marker fail closed; each needs its own regression,
because they exercise different `serde` failure modes and a careless impl could
pass one while regressing the other.

**Run both in the identity-gap (no-live-FSID) configuration, not the
at-risk-with-live-FSID one** -- the no-live-FSID setup is what makes "marker
removed" a real discriminator. Reuse the harness from
`cli/src/monitor.rs#cmd_monitor_identity_gap_fires_armed_and_keeps_baseline`:
`MonitorTestRunner::with_usage_and_override(usage_atrisk(),
MonitorOverride::BtrfsShowPayload(BTRFS_SHOW_2DISK_NO_UUID.to_owned()))` -- an
at-risk pool whose `btrfs show` carries no `uuid` line -- then seed the bad
marker by writing raw bytes to `paths.enospc_ack_json()` (as the corrupt /
old-shape marker tests do). Each asserts the same triple: the cycle fires
`EnospcRisk`, the marker file is **removed** best-effort, and no
`ComputationError` is folded.

Why no-live-FSID is mandatory: `evaluate_enospc_for_monitor` reaches the
corrupt-marker removal (the `load_enospc_ack` `Err` arm: remove + fire) *before*
any key comparison, so a correctly-rejected marker is always removed. But if a
careless impl instead *accepts* the bad marker (`#[serde(alias = "fs_uuid")]`,
`default`, or an `Option<Fsid>` field), it deserializes to `Ok(Some(..))` and
then routes by live key. With a live FSID present that lands in the
**key-mismatch** arm, which *also* removes + fires -- so the triple passes
spuriously and the regression slips through (exactly the `Option<Fsid>`/default
miss the plan must pin). With **no** live FSID, `live_pool_key` returns `None`,
so an accepted marker instead lands in the **identity-gap** arm, which fires
armed but deliberately **leaves the file in place** -- and the "marker removed"
assertion fails loudly. The existing `b"not json"` corrupt test
(`cmd_monitor_corrupt_baseline_fires_armed_without_computation_error`) already
covers the live-FSID corrupt path with an unparseable blob no regression can
accept, so these two add the discriminating coverage it cannot.

- **Legacy-key marker (the upgrade path).** Write *valid* JSON with the
  pre-rename schema: `pool_key.fs_uuid` (string) + `pool_key.devices` +
  `snoozed_until`. With `PoolKey.fsid` now a required field, the absent `fsid`
  key makes `serde` fail with "missing field `fsid`" -> corrupt-marker path
  (removed). This pins that the rename does **not** silently accept the old key:
  an `Option<Fsid>` field would deserialize it to `fsid: None` and, in this
  no-FSID cycle, leave the file in the identity-gap arm -- the "removed"
  assertion catches that. This is the test that backs the plan's headline
  upgrade-safety promise.
- **Malformed-fsid marker.** Write valid JSON whose `pool_key.fsid` is a non-UUID
  string (e.g. `"not-a-uuid"`); `Fsid`'s validating `Deserialize` rejects it ->
  corrupt-marker path (removed). This pins that a structurally-valid marker with a
  bad fsid fail-closes at load; a weakened `Deserialize` that accepted the garbage
  string would leave the file in the identity-gap arm, and the "removed"
  assertion catches that too.

## Out of scope (deliberate)

- Do **not** convert `AlertPoolState.fsid` to a non-optional `Fsid` or make the
  probe error on a missing FSID. The alert path intentionally tolerates absence
  (`probe_pool_alerts_tolerates_missing_fsid`); that asymmetry vs `probe_pool` is
  the point.
- No change to `PoolState.fsid`, `probe_fsid`, or any preflight/lock/replace code
  -- they already use `Fsid`.
- Do **not** rewrite the completed `plans/impl/*` records. A whole-repo
  `rg fs_uuid` also surfaces ~31 occurrences across four dated, promoted plans
  (most in `2026-06-19-proactive-enospc-monitor-alert.md`; `2026-06-22-enospc-baseline-geometry.md`
  even cites `cli/src/alert.rs#live_pool_key_requires_fs_uuid_and_sorts`, a test
  this refactor renames). Those files are immutable point-in-time history: dated
  filenames, outside the mdBook tree, and **not** scanned by any doc-citation
  guard -- `check-code-doc-anchors.py` and `check-plans-refs.py` both exclude
  `plans/` from their search roots, and neither resolves a `cli/src/*.rs#symbol`
  citation -- so the resulting stale reference breaks no check and is acceptable
  as history. This is why the step-4 verification grep is deliberately scoped to
  `cli/` and `docs/`, not the whole repo (mirroring the Python `get_fs_uuid()`
  carve-out).

## Verification

1. `just test-rust` -- the bulk of coverage is unit tests in `alert.rs`,
   `probe.rs`, `ack.rs`, `monitor.rs`; all listed behavioral tests plus the two
   new corrupt-marker regression tests (legacy-key and malformed-fsid, both in the
   identity-gap configuration) must pass. (Confirm `cargo clippy` is clean -- the
   `.as_deref()` -> `.as_ref()` change is the one spot that would otherwise fail
   to compile.)
2. No parser, btrfs-progs, or fixture-capture surface changes, so
   `just capture-all-fixtures` / `just test-parsers` are **not** required (the
   `enospc-ack.json` format is braid-owned, not a parsed-tool contract).
3. `just docs-build` -- **required**: it runs `mdbook build docs` with
   mdbook-linkcheck2. The plan edits ADR 014's marker-identity shorthand but
   preserves the `### Severity tiers and the ENOSPC baseline` heading, so the
   linkcheck confirms the `#severity-tiers-and-the-enospc-baseline` cross-links
   (from `018-systemd-lifecycle.md` and within `014-alerts.md`) still resolve.
   `check-see-paths.py` is **not** part of `docs-build` (it is a separate recipe /
   CI step) and is not exercised by this vocabulary-only body edit.
4. Completeness grep gate. The rename removes the lone `fs_uuid` token from the
   docs tree, so `rg -n 'fs_uuid' docs/` must return **zero**. Under `cli/`, one
   intentional occurrence survives: the **legacy-key regression test** in
   `cli/src/monitor.rs` deliberately writes the pre-rename JSON key, and proving it
   fails to deserialize *requires* emitting that literal `"fs_uuid"` (its value is
   the renamed `MONITOR_FSID` constant; only the JSON key stays `fs_uuid`). So
   `rg -n 'fs_uuid' cli/` must show **only** hits inside that one test -- its JSON
   literal, plus any comment or test-name referring to it; every other hit
   (production code, the `*_FS_UUID` fixtures now renamed to `*_FSID`) is a missed
   rename and must be zero. Do **not** discharge the gate by excluding
   `cli/src/monitor.rs` wholesale: that file still holds a production token
   (`evaluate_enospc_for_monitor`'s `fs_uuid: Option<&str>` parameter and its
   `live_pool_key` call, which must become `fsid: Option<&Fsid>`), so a blanket
   exclusion would hide a miss there -- audit the remaining `cli/` hits by eye and
   confirm each sits inside the legacy-key test body. Still out of scope, and
   already outside these grep paths: `tests/cli/braid-monitor-enospc-geometry.py`'s
   `get_fs_uuid()` helper reads the *real* btrfs FS UUID from `btrfs filesystem
   show` (regex on `uuid:`), not the `enospc-ack.json` key, so the rename never
   touches it.
