# Unify pool-device lookup-by-LUKS-UUID behind one `PoolState` accessor

## Context

A review finding flagged that `assert_new_uuid_unique` (plan-time) and
`verify_replace_execute_live_pool_uuid` (execute-time) both encode the same
live-pool collision predicate -- `pool.devices.iter().any(|d| d.luks_uuid == new_uuid)`
-- each producing `DuplicateUuid { scope: LivePool }`, so the rule can drift between
the dry-run preview and the real run. Investigation showed the duplication is wider
than the finding claimed: the "match a pool member by its LUKS UUID" predicate is
hand-written at **7 first-match/existence point-lookup sites** across `replace`,
`add`, `remove`, and `recover`, plus the `find_added_device_by_uuid` wrapper and the
existing `underlying_for_uuid` accessor that re-encode the same predicate again. That
accessor proves the codebase's idiom is a `PoolState` method -- the predicate simply
was never generalized.

LUKS UUID is the single-axis identity for *enrolled* pool membership (ADR 024
[`024-luks-uuid-identity.md`](../../docs/design/decisions/024-luks-uuid-identity.md)):
braid rejects a duplicate UUID before journal write. That is a write-time guarantee
about legitimate membership, **not** a property a live probe observes --
`probe.rs#probe_pool` records one row per live btrfs member with no UUID dedupe, so a
present cloned disk (same LUKS header, distinct backing/devid) is observable as
multiple `PoolState.devices` rows sharing a `luks_uuid`. The migrated sites already do
first-match (`.find()`) or existence (`.any()`) over that predicate, so they all want
the same first-match primitive; duplicate-sensitive scans (clone detection) stay
separate (see Out of scope).

**Goal:** introduce one accessor, `PoolState::device_by_uuid`, and route every
first-match point lookup by UUID through it. This collapses the drift surface to a
single definition and is purely behavior-preserving. It is a pivot away from the
finding's proposed `replace`-local `live_pool_has_uuid(...) -> bool`: one
`Option<&PoolDevice>` method serves both existence (`.is_some()`) and lookup needs,
matches the existing `underlying_for_uuid` / `PoolMembership::by_uuid` convention, and
is discoverable on the type.

## The change: one new primitive

In the existing `impl PoolState` block in `cli/src/types.rs`, alongside
`underlying_for_uuid`:

```rust
pub fn device_by_uuid(&self, uuid: &LuksUuid) -> Option<&PoolDevice> {
    self.devices.iter().find(|d| d.luks_uuid == *uuid)
}
```

Doc comment (per AGENTS.md, state *why at this boundary*, not the signature):
the shared **first-match** lookup of a live pool member by its LUKS UUID. It is the
one definition of the "first member with this UUID" predicate, so the rule cannot
drift across the plan-time, execute-time, and `add`/`remove`/`recover` surfaces;
existence callers use `.is_some()`.
Critical caveat to spell out in the comment: a `PoolState` comes from
`probe.rs#probe_pool`, which records one row per live btrfs member with **no UUID
dedupe**, so a present cloned disk is observable as multiple rows sharing a
`luks_uuid` and this helper returns only the first. Duplicate-sensitive code -- clone
detection (`add.rs#classify_live_pool_match`) -- must scan all rows itself and must
**not** use this helper. Do not restate ADR 024 as if UUIDs were unique within a live
`PoolState`; they are unique only among *enrolled* members at write time.

Then refactor the existing method to delegate (single source of truth for the scan) --
a **body-only** change that keeps its current `///` doc comment ("Live backing path...
Hardware queries must prefer this over persisted by-id paths") verbatim:

```rust
pub fn underlying_for_uuid(&self, uuid: &LuksUuid) -> Option<&str> {
    self.device_by_uuid(uuid).map(|d| d.underlying.as_str())
}
```

## Migration map

All transformations are mechanical and behavior-identical (`.find()`/`.any()` over
the same predicate). `uuid` arg forms already line up: pass `&LuksUuid` (existing
`*uuid` derefs become a plain reference).

**Existence sites (4) -> `pool.device_by_uuid(uuid).is_some()`:**

| Site (`cli/src/...`) | Function |
| --- | --- |
| `replace.rs` | `assert_new_uuid_unique` -- live-pool arm only (leave the membership arm) |
| `replace.rs` | `verify_replace_execute_live_pool_uuid` |
| `add.rs` | `assert_fresh_uuid_absent_from_live_pool` |
| `add.rs` | credential-verify dedup in the `AddCredentialPrelude` builder |

**First-match `.find()` sites (3) -> `pool.device_by_uuid(uuid)`:**

| Site (`cli/src/...`) | Function |
| --- | --- |
| `replace.rs` | `resolve_replace_source` (live old-disk lookup) |
| `recover.rs` | post-replace resize replay (`let Some(dev) = ... else { ... }`) |
| `remove.rs` | `plan_remove` (resolve target disk) |

**Absorb the redundant wrapper `find_added_device_by_uuid` (`add.rs`):**
It is mechanically identical to `device_by_uuid`, and its name ("added") is even
slightly inaccurate at its `recover` call sites (recover replays, it does not add).
Delete the wrapper and route its callers directly:

- Callers to repoint to `pool.device_by_uuid(...)`: two in `add.rs`
  (`cmd_add` post-add resolve; the `.is_none()` membership re-check) and two in
  `recover.rs` (`.ok_or_else(...)` resolves).
- Drop `use crate::add::find_added_device_by_uuid;` from `recover.rs`.
- Relocate + rename its test into the `cli/src/types.rs` test module (see Tests).

Deletion is the only planned path. A one-line delegating wrapper would leave two
shapes for the same lookup and undercut the goal of a single discoverable accessor.

## Explicitly out of scope (with rationale)

These share the predicate *text* but not the *intent*; routing them through a
"return the first match" accessor would be wrong or unclear:

- **`classify_live_pool_match` (`add.rs`) -- KEEP.** Its `.filter()` feeds a loop
  that OR-accumulates `different_backing` across *every* matching device. That is a
  deliberate fail-closed clone-detection guard against the exact pathology (two live
  rows sharing a UUID, which `probe.rs#probe_pool` produces without dedupe) that a
  first-match lookup like `device_by_uuid` assumes away. Collapsing it would weaken
  the guard (safety-heuristics fail-closed policy).
- **`build_member_verify_targets` (`replace.rs`) -- KEEP both arms.** This is
  set-partitioning: one arm filters `!= old_uuid` (a negation `device_by_uuid`
  cannot express), its sibling filters `== old_uuid` as the fallback anchor set.
  They are cohesive; splitting only the `==` arm would reduce local clarity.
- **Membership arm of `assert_new_uuid_unique` (`replace.rs`) -- KEEP.** It uses
  `PoolMembership::by_uuid` (a different type, already a method), not `PoolState`.
- **`source_has_io_errors` (`replace.rs`) -- not applicable.** It scans
  `BtrfsDeviceStatsOutput.devices` by `devid`, not `PoolState` by `luks_uuid`.

## Tests

- **Unit test for `device_by_uuid`** in the `cli/src/types.rs` test module, covering:
  hit (returns the matching `&PoolDevice`), miss (returns `None`), mapper-drift
  tolerance (matches on `luks_uuid` regardless of `mapper`), and a **duplicate-row
  case** -- two `PoolDevice` rows sharing one `luks_uuid` but with distinct
  `devid`/`underlying` (the cloned-disk shape `probe_pool` can produce) -- asserting
  the helper returns the *first* row. That last case pins the first-match contract so
  no future reader mistakes the helper for a uniqueness oracle. The existing
  `find_added_device_by_uuid_tolerates_drifted_mapper` test (`add.rs`) already asserts
  the hit/miss/drift trio -- relocate and rename it into `types.rs`, rewrite its `//`
  Intent/Why/Scenario preamble for the new name and the first-match/duplicate-row
  contract, then add the duplicate-row case, rather than writing a fresh test and
  deleting the old.
- **Must stay green unchanged** (structure-insensitive; these pin the migrated
  existence/collision arms): `replace_pre_write_uniqueness_membership_scope_collision`,
  `replace_pre_write_uniqueness_live_pool_scope_collision`,
  `replace_pre_write_uniqueness_excludes_old_uuid`,
  `execute_rechecks_live_pool_rejects_fresh_luks_uuid_collision` (and siblings),
  `fresh_uuid_live_pool_collision_omits_foreign_mapper`. The three first-match resolve
  sites (`resolve_replace_source`, `plan_remove`, recover-resize) and the
  credential-verify dedup are covered transitively through the `device_by_uuid` unit
  test -- a mechanical substitution into an identical-bodied, unit-tested helper does
  not warrant per-site regression tests.
- **Confirms the KEEP decisions hold** (untouched `classify_live_pool_match`): the
  `add` clone-detection suite -- `add_open_present_luks_same_uuid_different_backing_rejects_clone`,
  `..._same_backing_drift_noops`, and the closed-mapper variants. If any of these
  break, a filter site was collapsed that should not have been.

No new behavior is introduced, so no NixOS VM test is required.

## Verification

1. `just test-rust` -- runs the CLI unit-test suite (collision gates, the relocated
   `device_by_uuid` test, clone-detection suite). All green.
2. `cargo clippy --manifest-path cli/Cargo.toml -- -D warnings` -- no new lints; the
   `.is_some()` forms are idiomatic and should not trip `clippy`.
3. `cargo build --manifest-path cli/Cargo.toml` -- confirms the deleted wrapper has
   no stragglers and the dropped `use` in `recover.rs` is clean.
4. Not needed: parser fixtures (no parser change), VM tests (no behavior change), and
   the ASCII output check (no user-facing strings change).

## Risk notes

- Purely behavior-preserving: every migrated site keeps identical semantics and
  error types (`DuplicateUuid`/`AddError` construction stays at each call site -- only
  the scan predicate is shared).
- `device_by_uuid` is a first-match convenience, not a uniqueness oracle: a live
  `PoolState` can legitimately carry duplicate-UUID rows (cloned disk), so
  duplicate-sensitive scans stay out of scope and the helper's doc plus a unit case
  pin the first-match contract.
- The diff touches 5 files (`types.rs`, `replace.rs`, `add.rs`, `remove.rs`,
  `recover.rs`) but is small and uniform: one method added, one delegated, one
  wrapper removed, ~9 call sites rerouted.
