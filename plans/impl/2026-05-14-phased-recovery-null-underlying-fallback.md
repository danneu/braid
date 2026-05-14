# Plan: extend phased recovery's devid-fallback to `pool.null_underlying`

## Context

Commit `21ea1b6 fix(recover): preserve missing members in phased recovery`
taught `recover_membership_matching_expected` (cli/src/recover.rs:1622-1692)
to re-insert expected members whose live binding is devid-only via
`pool.missing_devids`. The companion gate `live_pool_matches_membership`
(cli/src/recover.rs:1586-1620) already accepted those bindings.

That fix intentionally did NOT broaden the same logic to
`pool.null_underlying`. The explicit comment at recover.rs:1658-1660
says so:

```rust
// pool.null_underlying is intentionally not consulted -- the gate doesn't
// gate on it today, and broadening this loop without broadening the gate
// would regress currently-accepted replays.
```

The follow-up work is to broaden BOTH the gate and the rebuild in lockstep.

### The bug / risk

Today, `pool.null_underlying` is honored only by the `OpKind::Remove`
guard at recover.rs:1080-1108. The phased recovery paths --
`OpKind::RemoveMissing` `PoolMutation` and `PostRemoveMissingMaintenance`
(call sites at recover.rs:2671, 2696, 2768) and `OpKind::Replace`
`PostReplaceMaintenance` (call site at recover.rs:3082-3120) -- route
through the helper pair and therefore IGNORE `pool.null_underlying`.

Concrete failure scenario:

1. Operator runs `braid remove-missing devid=2` against a healthy pool
   with disks `disk1`, `disk2` (target), `disk3`.
2. `btrfs device remove 2` commits; journal advances to
   `PoolMutation` with `target_membership = {disk1, disk3}`.
3. Before recovery rebuilds `pool.json`, `disk3`'s underlying block
   device is hot-unplugged. Its mapper stays open with `device: (null)`,
   so probe sorts `disk3` into `pool.null_underlying` (cli/src/probe.rs:302-410).
4. `live_pool_matches_membership(&pool, &target_membership)` computes
   `live_uuids = {disk1_uuid}` (only `pool.devices` is walked) and
   `missing_uuids = {}` (no MISSING sentinels). The union does not
   equal `{disk1_uuid, disk3_uuid}`, so the gate returns `Ok(false)`.
5. Recovery aborts with "remove-missing recovery found devid 2 gone,
   but live pool topology does not match the target membership".

The journal stays preserved (good), but the operator must now reconcile
manually -- even though `disk3`'s devid uniquely resolves through the
target membership exactly the way `pool.missing_devids` does.

This is the same shape of bug 21ea1b6 fixed for `missing_devids`. The
`OpKind::Remove` guard already proves that `null_underlying` is a
safe-to-tolerate identity source when the journaled devid resolves
uniquely; the phased paths should match.

### Authority

- **Principle 2** (CLI-owned membership, docs/principles.md:13-17): "when
  live btrfs reports a device by `devid` alone (the `null_underlying`
  mapper case and the btrfs `missing_devids` case), the persisted
  `devid` is the authorized fallback binding for re-attaching that live
  device to its membership entry."
- **Principle 5** (Stable identifiers, docs/principles.md:38-40): "When
  the live LUKS UUID is unobservable for a device the kernel/btrfs still
  reports (`null_underlying` mapper, btrfs `missing_devids`), btrfs
  `devid` is the only authorized live-fallback binding key."
- **Decision 017** (status Active): `added_at` precedence is
  current `pool.json` -> journal pre/target snapshot -> `now_iso()`.
- **Decision 024** (status Active): `DiskMember.devid is persisted only
  as prior-binding state for btrfs cases where the live device is
  observable by devid but not by LUKS UUID, such as null_underlying
  mappers and missing_devids.` Recovery must fail closed when a live
  btrfs device lacks an observable LUKS UUID and the journal has no
  persisted devid binding.

Both principles already authorize what this plan implements; the code
just hasn't caught up.

## Decision: gate semantics for `null_underlying`

In `live_pool_matches_membership`, `pool.null_underlying` is walked
alongside `pool.missing_devids` to build a single `fallback_uuids` set.
Each devid is resolved through `membership.by_devid()`. The gate also
tracks devids on both sides so it can fail closed on devid-level
collisions without relying on probe-layer invariants. The acceptance
predicate becomes:

```
live_uuids      ::= UUIDs from pool.devices (LUKS UUID observable)
live_devids     ::= devids from pool.devices
fallback_uuids  ::= {membership.by_devid(d).uuid
                      | d in pool.missing_devids
                              ∪ pool.null_underlying.map(|n| n.devid)}
fallback_devids ::= pool.missing_devids ∪ pool.null_underlying.map(|n| n.devid)
accept  iff  live_uuids ∪ fallback_uuids == expected_membership
       AND   live_uuids   ∩ fallback_uuids  == ∅
       AND   live_devids  ∩ fallback_devids == ∅
```

The devid-disjointness clause is defense-in-depth: probe today assigns
each mapper to exactly one of `pool.devices`, `pool.missing_devids`, or
`pool.null_underlying`, so the new clause cannot reject any currently
accepted state. It catches synthetic / probe-bug / test-fixture
states where the same devid surfaces on both sides and would
otherwise slip through UUID-only disjointness (when
`membership.by_devid(d)` resolves to a different UUID than the live
device that shares `d`).

Case-by-case answers to the five questions in the task brief:

1. **Null-underlying devid resolves uniquely through expected
   membership** -- accept the binding: insert the resolved UUID into
   `fallback_uuids` (and, in the rebuild, materialize the member with
   `added_at` precedence preserved). Same as the `missing_devids` path.

2. **Null-underlying devid has no expected member**
   (`expected.by_devid(devid) == Ok(None)`) -- fail closed. The gate
   raises `JournaledSnapshotError::NoMemberForDevid { devid }`; each
   call site bridges to `RecoverError::NoMemberForJournaledDevid`
   (cli/src/recover.rs:71-77). The journal is preserved. This matches
   Decision 024's "fail closed when journal has no persisted devid
   binding" rule and reuses existing error wording that already points
   the operator at `docs/luks-unlock.md` and
   `manual/guides/recovery-scenarios.md`.

3. **Null-underlying devid duplicates a missing devid** (same devid in
   both `pool.missing_devids` and `pool.null_underlying[*].devid`,
   the transient state acknowledged by
   `PoolState::alert_missing_devids` at types.rs:380-389) -- accept,
   idempotent. Both iterators resolve to the same UUID through
   `membership.by_devid()`; insertion into `fallback_uuids` (a
   `BTreeSet`) dedups. In the rebuild, the `by_uuid` short-circuit at
   recover.rs:1664 prevents a double-insert. No error.

4. **Null-underlying devid collides with a live device's UUID/devid**
   -- fail closed via two disjointness checks. If the resolved UUID
   appears in `live_uuids` (UUID collision), the UUID-disjoint check
   trips. If the null-underlying devid equals a live device's devid
   (devid collision) -- including the case where
   `membership.by_devid(devid)` resolves to a DIFFERENT UUID than the
   colliding live device's UUID, which UUID-disjoint alone would not
   catch -- the devid-disjoint check trips. In both cases the gate
   returns `Ok(false)` and each call site surfaces the per-phase "live
   pool topology does not match the target/pre membership" message
   that's already pinned. Better to fail than to silently
   mis-correlate.

5. **Membership contains duplicate devids** -- already covered:
   `membership.by_devid()` returns
   `MembershipError::DuplicateDevid { devid, members }`, which the gate
   re-raises as `JournaledSnapshotError::DuplicateDevid` and each call
   site bridges to `RecoverError::DuplicateDevidDuringReplay`
   (cli/src/recover.rs:63-70). Behavior is unchanged.

## Code changes

All changes are in `cli/src/recover.rs`. No new error variants. No
changes to `types.rs`, `membership.rs`, or any call site.

### 1. `live_pool_matches_membership` (cli/src/recover.rs:1586-1620)

Replace the `pool.missing_devids` loop with a single chained loop that
also walks `pool.null_underlying`, collecting into `fallback_uuids`
(renamed from `missing_uuids`):

```rust
fn live_pool_matches_membership(
    pool: &PoolState,
    membership: &PoolMembership,
) -> Result<bool, JournaledSnapshotError> {
    let live_uuids = live_member_uuids(pool);
    let live_devids: std::collections::BTreeSet<u64> =
        pool.devices.iter().map(|d| d.devid).collect();
    let mut fallback_uuids = std::collections::BTreeSet::new();
    let mut fallback_devids = std::collections::BTreeSet::new();
    for devid in pool
        .missing_devids
        .iter()
        .copied()
        .chain(pool.null_underlying.iter().map(|n| n.devid))
    {
        fallback_devids.insert(devid);
        match membership.by_devid(devid) {
            Ok(Some((uuid, _))) => {
                fallback_uuids.insert(uuid.clone());
            }
            Ok(None) => {
                return Err(JournaledSnapshotError::NoMemberForDevid { devid });
            }
            Err(membership::MembershipError::DuplicateDevid { devid, members }) => {
                return Err(JournaledSnapshotError::DuplicateDevid { devid, members });
            }
            Err(e) => {
                return Err(JournaledSnapshotError::NoMemberForDevid {
                    devid: match e {
                        membership::MembershipError::DuplicateDevid { devid, .. } => devid,
                        _ => devid,
                    },
                });
            }
        }
    }

    let expected: std::collections::BTreeSet<LuksUuid> =
        membership.iter().map(|(uuid, _)| uuid.clone()).collect();
    let union: std::collections::BTreeSet<LuksUuid> =
        live_uuids.union(&fallback_uuids).cloned().collect();
    let uuid_disjoint = live_uuids.is_disjoint(&fallback_uuids);
    let devid_disjoint = live_devids.is_disjoint(&fallback_devids);
    Ok(union == expected && uuid_disjoint && devid_disjoint)
}
```

Name change rationale: `fallback_uuids` matches principle 5 ("btrfs
`devid` is the only authorized live-fallback binding key") and the
already-present comment in the rebuild function. Keeping
`missing_uuids` would be actively misleading after the fix.

### 2. `recover_membership_matching_expected` (cli/src/recover.rs:1622-1692)

Widen the second loop's iterator to the same chained source and update
the leading comment. The body is unchanged: `expected.by_devid()`
resolves the binding, `recovered.by_uuid(uuid).is_some()` short-circuits
idempotently (handling case 3), the `added_at` precedence chain
(prior -> expected -> `now_iso()`) is preserved per Decision 017.

```rust
    // Re-insert any expected member whose live binding is devid-only.
    // Per principles 2/5, btrfs devid is the authorized fallback when
    // the LUKS UUID is unobservable -- the two devid-only sources are
    // pool.missing_devids (btrfs-MISSING sentinels) and
    // pool.null_underlying (hot-unplugged mappers). The
    // live_pool_matches_membership gate has already proven every such
    // devid resolves uniquely through expected; this loop materializes
    // that resolution in the rebuilt membership. The by_uuid
    // short-circuit makes the loop idempotent in the rare case the
    // same devid appears in both sources.
    for devid in pool
        .missing_devids
        .iter()
        .copied()
        .chain(pool.null_underlying.iter().map(|n| n.devid))
    {
        match expected.by_devid(devid) {
            Ok(Some((uuid, expected_member))) => {
                if recovered.by_uuid(uuid).is_some() {
                    continue;
                }
                let added_at = prior
                    .and_then(|p| p.by_uuid(uuid))
                    .and_then(|m| m.added_at.clone())
                    .or_else(|| expected_member.added_at.clone())
                    .or_else(|| Some(crate::util::now_iso()));
                recovered.insert(
                    uuid.clone(),
                    DiskMember {
                        added_at,
                        ..expected_member.clone()
                    },
                )?;
            }
            Ok(None) => {
                return Err(RecoverError::NoMemberForJournaledDevid { devid });
            }
            Err(membership::MembershipError::DuplicateDevid { devid, members }) => {
                return Err(RecoverError::DuplicateDevidDuringReplay { devid, members });
            }
            Err(err) => {
                return Err(RecoverError::Membership(err));
            }
        }
    }
```

### What this fix does NOT touch

- `cli/src/types.rs`: `PoolState`, `NullUnderlyingDevice`,
  `alert_missing_devids` -- unchanged. `null_underlying` and
  `missing_devids` stay as separate fields because the rest of the
  codebase distinguishes them for `remove-missing` target resolution
  (see types.rs:367-378).
- `cli/src/membership.rs`: `by_devid` already returns the exact
  `Result<Option<(&LuksUuid, &DiskMember)>, MembershipError>` shape we
  need; reuse as-is.
- Error variants: `JournaledSnapshotError`,
  `RecoverError::NoMemberForJournaledDevid`,
  `RecoverError::DuplicateDevidDuringReplay`,
  `RecoverError::Membership` already cover every case.
- The `OpKind::Remove` guard at recover.rs:1080-1108 stays as-is. It
  walks `plan.journal.pre_membership` and matches journaled member
  devids against live `pool.null_underlying` + `pool.missing_devids`,
  which is a different shape from the helper pair (the guard restores
  pre_membership disks the live rebuild dropped; the helpers walk live
  state and look up expected). Keeping them separate preserves the
  guard's mapper-name-fallback refusal at recover.rs:1096-1106, which
  is required by Decision 024.
- All call sites (recover.rs:2671, 2696, 2768, 3082, 3107) inherit the
  fix transparently; no changes there.

## Tests

All tests live in `cli/src/recover.rs`'s `#[cfg(test)]` module and
reuse existing helpers:

- `pool_state_disk1_with_null_underlying_disk2` (recover.rs:5218)
- `pool_state_two_disks` (search nearby) /
  `pool_state_three_disks` (recover.rs:5307)
- `membership_from`, `membership_entry`, `disk_member_named`
  (recover.rs:4565, 4579, 4589)
- `PoolMembership::for_corruption_tests` (membership.rs:395,
  `pub(crate)`) -- required by T7 because the normal `insert` path
  rejects duplicate non-`None` devids
- `resolver_for` (recover.rs:3846)
- `uuid_for_name`, `disk_name`
- `PoolFixture` + `cmd_recover` + `MockRunner` patterns used by
  `cmd_recover_remove_missing_pool_mutation_preserves_non_target_missing_disk`
  (recover.rs:14661)

Each test starts with the three-section preamble (Intent / Why it
exists / Scenario) per the project test conventions.

### T1: `live_pool_matches_membership_accepts_null_underlying_devid`

- **Intent**: gate returns `Ok(true)` when an expected member's only
  live binding is via `pool.null_underlying[*].devid` and that devid
  resolves uniquely.
- **Why it exists**: pins the gate broadening; without the fix the
  gate returns `Ok(false)` and phased recovery aborts.
- **Scenario**: pool = `pool_state_disk1_with_null_underlying_disk2`;
  expected = two members, disk1 with `devid: Some(1)`, disk2 with
  `devid: Some(2)`.
- **Assert**: gate returns `Ok(true)`.

### T2: `live_pool_matches_membership_rejects_null_underlying_without_expected_devid`

- **Intent**: gate fails closed when a `null_underlying` devid has no
  matching member in expected (Decision 024).
- **Why it exists**: pins the fail-closed semantics for case 2 of the
  decision matrix.
- **Scenario**: pool has `null_underlying` entry with `devid: 99`;
  expected has disks with `devid: 1` and `devid: 2` only.
- **Assert**: gate returns
  `Err(JournaledSnapshotError::NoMemberForDevid { devid: 99 })`.

### T3: `recover_membership_matching_expected_reinserts_null_underlying_member`

- **Intent**: rebuild materializes a `null_underlying` binding into
  recovered membership, preserving `added_at` precedence per
  Decision 017.
- **Why it exists**: direct mirror of
  `recover_membership_matching_expected_reinserts_missing_devid_member`
  (recover.rs:10653); pins the rebuild broadening.
- **Scenario**: pool has disk1 + disk2 in `devices`, disk3 in
  `null_underlying` (devid 3); expected has all three with
  `expected_added_at`; prior `pool.json` has disk3 with
  `prior_added_at`.
- **Assert**: `recovered.by_uuid(disk3_uuid).added_at ==
  Some(prior_added_at)`, `devid == Some(3)`, name and by_id from
  expected.

### T4: `recover_membership_matching_expected_dedups_missing_and_null_underlying_devid`

- **Intent**: same devid in `pool.missing_devids` and
  `pool.null_underlying` resolves to one membership entry, not two.
- **Why it exists**: pins case 3 of the decision matrix (the transient
  state already acknowledged by `alert_missing_devids` at
  types.rs:380-389).
- **Scenario**: pool has `missing_devids = vec![2]`,
  `null_underlying = vec![NullUnderlyingDevice { devid: 2, ... }]`;
  expected has disk1 (devid 1) and disk2 (devid 2).
- **Assert**: `recovered.by_uuid(disk2_uuid).is_some()`, exactly one
  entry materialized; no `RecoverError`.

### T5: `cmd_recover_remove_missing_pool_mutation_preserves_non_target_null_underlying_disk`

- **Intent**: end-to-end `cmd_recover` for an interrupted
  `RemoveMissing` `PoolMutation` preserves a non-target disk that has
  flapped to `null_underlying`; `pool.json` is rewritten and the
  journal is cleared.
- **Why it exists**: direct mirror of
  `cmd_recover_remove_missing_pool_mutation_preserves_non_target_missing_disk`
  (recover.rs:14661); pins the fix at the command boundary, so a
  revert at either the gate or the rebuild surfaces here.
- **Scenario**: 3-disk pool, `braid remove-missing devid=2` committed,
  then `disk3`'s underlying block device hot-unplugs between the btrfs
  commit and the pool.json rewrite. Live pool: `disk1` in `devices`,
  `disk3` in `null_underlying` (cryptsetup status `device: (null)`),
  `disk2` gone. Journal: `RemoveMissing { devid: 2, ... }` in
  `PoolMutation` phase with `target_membership = {disk1, disk3}` and
  enriched devids.
- **Assert**: `recovered.by_name(disk1)` Some,
  `recovered.by_name(disk2)` None, `recovered.by_name(disk3)` Some,
  pending-op cleared.

### T6: `live_pool_matches_membership_rejects_null_underlying_devid_colliding_with_live_devid`

- **Intent**: gate returns `Ok(false)` when the same devid surfaces in
  both `pool.devices` and `pool.null_underlying`, even if UUID-only
  disjointness would have passed.
- **Why it exists**: pins the devid-disjointness clause added to the
  acceptance predicate. Without it, a synthetic / probe-bug state
  where `membership.by_devid(d)` resolves to a UUID different from the
  colliding live device's UUID could pass the gate and surface later
  as a less precise membership conflict.
- **Scenario**: pool has live device `{ devid: 2, luks_uuid: B }` and
  a `null_underlying` entry with `devid: 2`; expected membership
  contains a member with `devid: Some(2)` whose UUID is some `D != B`,
  plus enough other entries that the union equality would otherwise
  hold.
- **Assert**: gate returns `Ok(false)` (no error -- this is a
  topology mismatch, not journal corruption).

### T7: `live_pool_matches_membership_propagates_duplicate_devid_from_null_underlying`

- **Intent**: gate surfaces `JournaledSnapshotError::DuplicateDevid`
  when a `null_underlying` devid resolves to two or more expected
  members.
- **Why it exists**: the chained-iterator rewrite reuses the existing
  duplicate-devid arm, but until this test the new iterator source
  was not exercised through it; a regression that mishandled the
  `MembershipError::DuplicateDevid` mapping for the
  `pool.null_underlying` source would not be caught by any other test
  in this plan.
- **Scenario**: pool has `null_underlying = [NullUnderlyingDevice {
  devid: 2, ... }]`; expected membership contains two members both
  with `devid: Some(2)`. `PoolMembership::insert` rejects duplicate
  non-`None` devids
  ([cli/src/membership.rs:339](cli/src/membership.rs)), so this
  fixture cannot be built with `membership_from` /
  `membership_entry`. Use the existing `#[cfg(test)]` constructor
  `PoolMembership::for_corruption_tests(entries)`
  ([cli/src/membership.rs:395](cli/src/membership.rs), `pub(crate)`
  so it is reachable from `recover.rs`'s test module) to assemble
  the corrupt expected membership.
- **Assert**: gate returns `Err(JournaledSnapshotError::DuplicateDevid
  { devid: 2, members })` where `members.len() == 2`.

### T8: `cmd_recover_replace_post_maintenance_preserves_non_target_null_underlying_disk`

- **Intent**: end-to-end `cmd_recover` for an interrupted Replace that
  has committed (new disk live) preserves a non-target disk in
  `null_underlying` AND completes the Replace-specific post-helper
  logic at recover.rs:3244 (locating `new_uuid` in `pool.devices` to
  replay the resize).
- **Why it exists**: T1+T3 pin the helper behavior, but Replace has
  path-specific code AFTER `recover_membership_matching_expected`
  that T5 does not exercise. A revert at the helpers OR a future
  change that re-discovers `new_uuid` via the rebuilt membership
  (rather than from `pool.devices`) would surface here.
- **Scenario**: 3-disk pool starting as `{disk1, disk-old, disk3}`;
  `braid replace disk-old disk-new` committed (`new_uuid` live,
  `old_uuid` not). Between commit and `pool.json` rewrite, `disk3`'s
  underlying block device hot-unplugs. Live pool: `disk1` and
  `disk-new` in `devices`, `disk3` in `null_underlying`. Journal:
  Replace with `target_membership = {disk1, disk-new, disk3}` and
  enriched target devids, in a phase that routes through
  `execute_replace_post_maintenance_recovery`. The fixture leaves the
  old pre-member's `devid` unset because `cmd_recover` builds a union
  of replace pre/target snapshots before dispatch, while
  `ReplaceJournalSource::Missing { old_devid: 2 }` still carries the
  old binding.
- **Assert**: `recovered.by_name(disk1)` Some,
  `recovered.by_name(disk-new)` Some,
  `recovered.by_name(disk-old)` None, `recovered.by_name(disk3)` Some,
  the post-replace resize ran on `disk-new`'s live devid, pending-op
  cleared.

T1-T4 + T6-T7 catch reverts to the helper functions directly. T5 and
T8 catch reverts at the command boundary -- T5 for RemoveMissing
post-commit, T8 for Replace post-commit (including the post-helper
`new_uuid` lookup at recover.rs:3244). T2 and T7 together preserve
Decision 024's fail-closed boundary against any regression that would
"always accept null_underlying".

## Verification

Run from repo root:

```
just test-rust
```

Expected: existing parser/recover tests pass and the eight new tests
(T1-T8) pass. Note the CLI crate is `braid-cli`; `just test-rust` is
the canonical invocation.

Optional manual cross-check before the unit pass: open recover.rs and
confirm the comment block at the top of the rebuild loop no longer
says "pool.null_underlying is intentionally not consulted".

## Critical files

- `cli/src/recover.rs`
  - `live_pool_matches_membership` (lines 1586-1620): widen iterator,
    rename set.
  - `recover_membership_matching_expected` (lines 1622-1692): widen
    iterator, replace stale comment.
  - `#[cfg(test)]` module: add T1-T8 reusing existing helpers
    (`membership_from`, `membership_entry`, `resolver_for`,
    `PoolFixture`, `pool_state_disk1_with_null_underlying_disk2`,
    etc.) plus `PoolMembership::for_corruption_tests` for T7's
    corrupt-expected fixture.

No other source files change.
