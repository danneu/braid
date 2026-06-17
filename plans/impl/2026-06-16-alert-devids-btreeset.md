# Plan: make `AlertDevids` carry `BTreeSet<Devid>` instead of `Vec<Devid>`

## Context

`AlertPoolState::alert_devids()` (`cli/src/probe.rs#alert_devids`) builds its two
fields by collecting into a `BTreeSet<Devid>` and then throwing the set-ness away
with `.into_iter().collect()` back into a `Vec`. The fields are therefore
*semantically* deduplicated, ordered sets, but the `Vec` type does not say so. As
a result the "deduped, ordered set" invariant lives only in a doc comment and is
silently re-asserted at every consumer: four production sites rebuild a
`BTreeSet` from these vecs purely for membership lookups.

A code-review finding flagged one of those rebuilds (`compute_alert_state`,
`cli/src/alert.rs:111-112`) as redundant work and an extra mental hop. The
minimal fix it proposed (slice `.contains`, or a defensive comment) leaves the
field a `Vec`, so the invariant stays doc-only and the three sibling rebuilds
survive. The ideal fix is structural: change the field types to
`BTreeSet<Devid>` so the invariant is carried by the type and cannot be
violated. This dissolves all four rebuilds, simplifies the producer, and aligns
with the project rule "reach for the ideal, robust, simple, most correct
solution -- regardless of refactor cost" (`AGENTS.md`).

Outcome: identical behavior (the vecs were already sorted+deduped, iteration
order is unchanged, `MissingDevice` causes still emit in ascending devid order),
fewer lines, and an invariant that is now type-enforced.

## Preconditions (verified during investigation)

- `Devid` derives `Ord` (`cli/src/types.rs#Devid`), so `BTreeSet<Devid>` is valid.
- `AlertDevids` has no derives and no `Serialize`/`Deserialize` -- it is a
  transient in-memory carrier, so there is no wire-format constraint.
- `BTreeSet` is already imported in `probe.rs`, `alert.rs`, and `monitor.rs`.
- `reconcile_acked_stats` already takes `&BTreeSet<Devid>`
  (`cli/src/alert.rs#reconcile_acked_stats`).
- No consumer uses these fields in a `Vec`-specific way (no indexing, mutation,
  `&[Devid]`/`&Vec` parameter, or serialization). Only uses are: membership
  (`.contains`), iteration (`for &devid in ...`), re-collection into a
  `BTreeSet`, passing the whole carrier by reference to `snapshot_current` /
  `compute_alert_state` (the `cmd_ack` and `monitor` paths), and equality
  assertions in tests.

## Approach

### 1. Change the type (`cli/src/probe.rs`)

In `struct AlertDevids` (`cli/src/probe.rs#AlertDevids`), change both fields from
`Vec<Devid>` to `BTreeSet<Devid>`.

In `alert_devids()` (`cli/src/probe.rs#alert_devids`), drop the trailing
`.into_iter().collect()` on both fields so each expression ends at
`.collect::<BTreeSet<Devid>>()` and is stored directly. Net: two lines removed
per field, no behavior change.

Tighten the two field doc comments: the `missing` field doc currently says
"deduplicated" and the prose implies ordering -- reword so the comment describes
*intent* (which devids belong in each set) and lets the `BTreeSet` type carry the
dedup/order invariant rather than restating it. The struct-level doc
(swap-prevention rationale) and the `alert_devids()` doc stay valid as-is.

### 2. Drop the four production rebuilds (the payoff)

- `cli/src/alert.rs#compute_alert_state` (lines ~111-112): delete the two
  `let recognized: BTreeSet<Devid> = ...` / `let missing: BTreeSet<Devid> = ...`
  rebuilds. In the loop body use `devids.missing.contains(&dev.devid)` and
  `devids.recognized.contains(&dev.devid)` directly (now `BTreeSet::contains`,
  O(log n)).
- `cli/src/alert.rs#snapshot_current` (line ~179): delete the `recognized`
  rebuild; use `devids.recognized.contains(&dev.devid)` directly.
- `cli/src/monitor.rs` (line ~102): delete the
  `let still_relevant_devids: BTreeSet<_> = devids.recognized.iter()...` line and
  pass `&devids.recognized` directly to `reconcile_acked_stats(&mut acked,
  &devids.recognized, &present_devids)`.

The two iteration sites (`for &devid in &devids.missing` at `cli/src/alert.rs`
~131 and ~204) need no change -- `&BTreeSet` iterates in ascending order, matching
the prior sorted-`Vec` order.

### 3. Update test construction + assertion sites (mechanical churn)

These are the only code-breaking sites; all are tests.

- ~17 inline `AlertDevids { recognized: vec![...], missing: vec![...] }`
  constructors in `cli/src/alert.rs` tests: change each `vec![...]` field to
  `BTreeSet::from([...])`. Representative lines: 1028, 1048, 1071, ... 1441.
- Special case at `cli/src/alert.rs#null_underlying_device_triggers_missing_alert`
  (~1883-1890): the test binds `let alert_missing = vec![...]; let recognized =
  vec![...];` then `.clone()`s them into the struct. Change those two locals to
  `BTreeSet::from([...])` (the `.clone()` stays valid).
- probe.rs assertions comparing the fields to `vec![...]`
  (`assert_eq!(state.alert_devids().missing, vec![...])` etc. at
  `cli/src/probe.rs` ~2209, ~2240, ~2280, ~2285): change the expected `vec![...]`
  to `BTreeSet::from([...])`. `BTreeSet<Devid>` does not impl `PartialEq` with
  `Vec`, so these must change to compile.

Coverage note: these probe.rs tests previously asserted equality against a
*sorted* `Vec`, implicitly checking sort/dedup. After the change, sort+dedup is
structurally guaranteed by `BTreeSet`, so set-equality assertions are the correct
and complete form -- no coverage is lost. The dedup-intent comment at
`cli/src/probe.rs` (~2186-2187, "deduplication and sorting are part of the alert
cause contract") stays valid and is now type-enforced; leave it, or add a short
note that the `BTreeSet` field type now enforces it.

### 4. Docs

- ADR 014 `docs/design/decisions/014-alerts.md#ack-state-keyed-by-btrfs-devid`
  (the paragraph at line ~66): the prose describes the `recognized` field as "the
  union of `present_devids`, `null_underlying`, and `missing_devids`" and the
  filtering behavior -- this stays accurate under the type change. Re-read to
  confirm; no edit is expected. Do not add line numbers; if a touch is warranted,
  keep it to noting the carrier's fields are sets.
- No `principles.md` change required; this change is consistent with principle 3
  (safe-by-construction / invariant placement) by making the dedup/order
  invariant non-representable-when-wrong.

## Out of scope

- `cli/src/monitor.rs` (~line 101) builds `present_devids` from
  `pool.present_devids`, which is a `Vec<Devid>` field on the separate
  `AlertPoolState` struct -- a genuine set-from-non-set construction, not the same
  redundancy. Leave it.
- Do not change `AlertPoolState`'s own field types.

## Files to modify

- `cli/src/probe.rs` -- struct field types, `alert_devids()` body, field doc
  comments, ~4 test assertions.
- `cli/src/alert.rs` -- delete 3 rebuilds in `compute_alert_state` /
  `snapshot_current`, ~17 test constructors, 1 special-case test.
- `cli/src/monitor.rs` -- delete 1 rebuild, pass `&devids.recognized` directly.
- `cli/src/ack.rs` -- no edit. `cmd_ack_impl` (`cli/src/ack.rs#cmd_ack_impl`, the
  mounted `braid ack` path) calls `alert_devids()` then passes the carrier to
  `snapshot_current`; it never reads `.recognized`/`.missing` directly, so the
  type change is transparent here. Listed so the affected surface is fully
  inventoried, and exercised in verification below.
- `docs/design/decisions/014-alerts.md` -- verify only; edit unlikely.

## Verification

1. `cargo fmt --manifest-path cli/Cargo.toml` then
   `cargo fmt --manifest-path cli/Cargo.toml -- --check`.
2. `cargo clippy --manifest-path cli/Cargo.toml --all-targets` -- expect no new
   warnings; confirm no now-unused `BTreeSet`/`iter` imports remain.
3. `just test-rust` (or `cargo test --manifest-path cli/Cargo.toml`) -- all unit
   tests pass. Focus on the alert/probe modules:
   - `compute_alert_state` suite (`no_alert_when_all_zero`,
     `alert_on_btrfs_device_errors`, `alert_on_missing_device`,
     `missing_devid_*`, `unrecognized_devid_with_errors_does_not_alert`, ...).
   - `snapshot_current` suite (`snapshot_current_captures_stats`,
     `snapshot_current_preserves_null_underlying_stats`).
   - `alert_devids()` suite (`probe_pool_alerts_alert_devids_missing`,
     `probe_pool_alerts_recognized_devids`,
     `alert_devids_carrier_routes_each_origin_correctly`).
   - `monitor` reconcile path (`reconcile_acked_stats_*`).
   - `cmd_ack` path (mounted `braid ack` -> `snapshot_current`):
     `cmd_ack_does_not_persist_unrecognized_devid_in_acked_stats` (unrecognized-
     devid filtering) and `ack_baseline_suppresses_then_refires_btrfs_device_errors`
     (acked baseline persistence).
4. `scripts/docs/check-output-ascii.py` is unaffected (no user-facing output
   strings change), but run the repo's standard pre-commit/CI lane if convenient.
5. If `docs/design/decisions/014-alerts.md` is touched, run `just docs-build` so
   `mdbook-linkcheck2` validates links.

## Why this over the finding's proposal

The finding proposed slice `.contains` on a still-`Vec` field or a defensive
comment. Both leave the dedup/order invariant in prose and fix only the one cited
rebuild, leaving the `snapshot_current` and `monitor` siblings. Encoding the
invariant in the type fixes all four sites at once, simplifies the producer, and
makes the redundant rebuild un-writable going forward -- a structural fix that
dissolves the class of issue rather than patching one instance.

## Implementation notes

- Empty-set test constructors use `BTreeSet::new()` rather than the plan's
  literal `BTreeSet::from([...])`. `BTreeSet::from([])` cannot infer the element
  type from an empty array literal, and `new()` is the idiomatic empty form;
  non-empty constructors use `BTreeSet::from([...])` as the plan specified.
