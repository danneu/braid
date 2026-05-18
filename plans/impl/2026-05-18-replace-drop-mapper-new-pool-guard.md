# Plan: Delete the late mapper-keyed "new disk not in pool" guard

## Context

`ReplacePlan::execute` calls `check_new_not_in_pool` just before the
sleep inhibitor and journal write:

```rust
// Guard: new disk must not already be in the pool.
check_new_not_in_pool(new_name.as_str(), &new_mn, &pool)?;
```

The helper checks the reconstructed replacement mapper name against
`pool.devices`:

```rust
if pool.devices.iter().any(|d| d.mapper == *new_mn) {
    return Err(...);
}
```

That predicate is a mapper-keyed pool-membership decision, which
violates the UUID identity rule in
[`docs/principles.md`](../../docs/principles.md#5-stable-identifiers):
"No code path may decide membership, target a device, or correlate
live pool state by parsing a name out of a mapper path or LUKS label."

The plan-time guard `assert_new_uuid_unique` (called from
`plan_replace` at `replace.rs:1359`) already enforces the
same-pool/no-collision invariant on the UUID axis:

```rust
// assert_new_uuid_unique LivePool arm:
if pool.devices.iter().any(|d| d.luks_uuid == *new_uuid) {
    return Err(ReplaceError::DuplicateUuid {
        uuid: new_uuid.clone(),
        scope: DuplicateUuidScope::LivePool,
    });
}
```

Between `plan_replace` and `ReplacePlan::execute`, `pool` is frozen on
`ReplaceWorkPlan` and is not re-probed. Therefore, the late guard's
predicate (in UUID-keyed form) is mathematically shadowed by
`assert_new_uuid_unique`: any state that would trip the late guard has
already tripped `assert_new_uuid_unique` at plan time. The late guard
is unreachable from `cmd_replace`.

The VM test `tests/cli/replace-new-in-pool-guard.py` confirms this
empirically: it asserts the `DuplicateUuid { Membership }` error
("duplicate LUKS UUID ... already present in membership") from
`assert_new_uuid_unique`, not the late guard's "already a member"
message.

## Intended outcome

The mapper-keyed pool-membership check at `replace.rs:553` is removed
along with the helper that backs it. No new mapper-keyed pool-membership
decision replaces it. UUID-keyed enforcement at `plan_replace` remains
the single source of truth for "new disk must not already be in the
pool."

## Approach

All code changes stay in `cli/src/replace.rs`.

1. Delete the call site:

   ```rust
   // Guard: new disk must not already be in the pool.
   check_new_not_in_pool(new_name.as_str(), &new_mn, &pool)?;
   ```

   at `replace.rs:552-553`.

2. Delete the helper at `replace.rs:1570-1582`.

3. Delete the two helper unit tests at `replace.rs:2396-2421`
   (`new_disk_already_in_pool_rejected`,
   `new_disk_not_in_pool_passes`); they exercise the deleted helper.

4. Update the doc comment on `plan_replace` at `replace.rs:1117-1121`
   to drop the `check_new_not_in_pool` reference. The sentence becomes:

   ```text
   /// Does not read or verify the passphrase or acquire the sleep
   /// inhibitor -- those happen inside `ReplacePlan::execute` so
   /// `--dry-run` keeps short-circuiting before them.
   ```

5. Adjust the inhibitor-placement comment at `replace.rs:562-567` to
   drop the now-deleted helper from its list of "reversible work"
   examples. Replace the parenthetical
   `(confirmation, passphrase read+verify, check_new_not_in_pool)`
   with `(confirmation, passphrase read+verify)`.

## Verification

1. `just test-rust` -- proves the helper deletion does not break any
   surviving unit test, and that `assert_new_uuid_unique`'s existing
   tests at `replace.rs:5701-5833` continue to cover the membership
   and live-pool arms.
2. `just test-vm replace-new-in-pool-guard replace-live-disk replace-dead-disk`.
   `replace-new-in-pool-guard` already asserts the rejection comes
   from `assert_new_uuid_unique` (`"duplicate LUKS UUID"` +
   `"already present in membership"`) and that no journal is stranded
   and pool.json is bit-identical; that asserts the surviving guard
   path is the correct seam. The live and missing replace checks
   confirm the broader replace flow still works.

No new test is required. The behavior being preserved (reject new
disks already in the pool) is already covered by
`assert_new_uuid_unique`'s LivePool arm and its existing unit test at
`replace.rs:5755-5805` (`assert_new_uuid_unique_rejects_uuid_collision_with_live_pool_member`).
The behavior being removed (the late mapper-keyed shadow) is dead code
and has no observable runtime effect.

## Why this dissolves the structure-coupled-test concern

A previous draft of this plan re-keyed the helper from mapper to UUID
and kept it as a defense-in-depth pre-journal assertion. That draft
created a coverage gap: the only way to actually exercise the late
guard is to construct `ReplacePlan` directly, bypassing `plan_replace`
-- because `assert_new_uuid_unique` shadows the predicate for any pool
state reachable through `cmd_replace`. Constructing `ReplacePlan` from
scratch in a unit test requires hand-building all 16 fields of the
private `ReplaceWorkPlan` (config, uuids, names, by-id, pool,
replace_source, target_prep, both memberships, both journal halves,
restore flag, mapper, mapper path), coupling the test to internal
struct shape. Deleting the dead guard removes the need for that
scaffolding without weakening any reachable invariant.

## Out of scope

- Do not delete or weaken `assert_new_uuid_unique`; it remains the
  primary pre-plan UUID collision guard and the only enforcement of
  "new disk must not already be in the pool" after this change.
- Do not change the already-open ExistingLuks execute-time mapper
  verification at `replace.rs:758`; that is covered by a separate plan.
