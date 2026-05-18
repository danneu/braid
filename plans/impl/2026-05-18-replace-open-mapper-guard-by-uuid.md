# Plan: Always verify the already-open replace target mapper

## Prerequisites

This plan depends on
`plans/wip/replace-check-new-not-in-pool-by-uuid.md` landing first.

That plan deletes `check_new_not_in_pool` (the helper and its call
site at `replace.rs:553`) entirely, leaving `assert_new_uuid_unique`
in `plan_replace` as the sole "new disk must not already be in the
pool" enforcement. Without that deletion first, the regression test
below cannot reach the verifier branch this plan modifies: the test's
pool shape (a `pool.devices` row with `mapper == new_mn` and a
different `luks_uuid` than `new_uuid`) would be rejected by the
mapper-keyed `check_new_not_in_pool` before execute ever reaches
`replace.rs:758`, so the test would fail for the wrong reason and
prove nothing about the deletion in this plan.

Sequencing also matters for code-shape reasons. In current code, the
skip is dead (see Context); deleting it has no behavioral consequence
on its own. The deletion only acquires a behavioral consequence once
the sibling plan removes `check_new_not_in_pool`, at which point a
drifted-mapper pool row (`mapper == new_mn`, `luks_uuid != new_uuid`)
would pass the remaining UUID-keyed plan-time guard and -- if the
skip here were still present -- bypass the verifier. Landing this
plan immediately after the sibling closes that window in the same
release cycle.

## Context

In the `ExistingLuks { mapper_open: true }` execute path,
`ReplacePlan::execute` decides whether to re-verify the already-open
replacement mapper before `btrfs replace start`:

```rust
} else if !pool.devices.iter().any(|d| d.mapper == new_mn) {
    verify_existing_luks_open_mapper_target(...)?;
    eprintln!(
        "note: LUKS mapper is already open but device is not yet in pool. Completing replace."
    );
}
```

The verifier itself is good: `verify_existing_luks_open_mapper_target`
uses `classify_mapper_ownership`, which requires the mapper backing path
and LUKS UUID to match the configured replacement target. The problem is
the skip condition. It treats a mapper-name collision in `pool.devices`
as proof that no verification is needed -- the exact shape Decision 024
warns against, since mapper names are runtime handles, not membership
identity.

In current code, this skip is dead. By the time execute reaches this
branch, two earlier guards have already eliminated every state in which
the predicate can be true:

- `plan_replace` runs `assert_new_uuid_unique` (replace.rs:1359), which
  refuses any plan where `new_uuid` appears in `pool.devices`.
- `ReplacePlan::execute` runs `check_new_not_in_pool` (replace.rs:553),
  which refuses execution when any `pool.devices` row has
  `mapper == new_mn`.

After both pass, no pool row can have `mapper == new_mn`, so the
`else if` predicate is always true and the verifier always runs. The
skip exists only to express an idempotency intent that the surrounding
preflight already prevents from ever being reachable.

After the sibling plan lands and `check_new_not_in_pool` is deleted,
the mapper-name skip here stops being dead and becomes actively wrong.
A pool row with `mapper == new_mn` but `luks_uuid != new_uuid` is no
longer rejected by any execute-time pool guard (only the plan-time
UUID-keyed `assert_new_uuid_unique` remains, and it passes for
`luks_uuid != new_uuid`). That row would reach the mapper-keyed skip
here, fire it, bypass the verifier, and let `btrfs replace start` run
against a foreign open mapper. Deleting the skip in this plan closes
that window the moment the sibling plan opens it.

## Intended outcome

The already-open ExistingLuks path always runs
`verify_existing_luks_open_mapper_target` before `btrfs replace start`.
No skip condition gates the verifier.

The "device is not yet in pool" stderr note is removed along with the
skip, because the unconditional verifier already proves the open mapper
matches the configured target by backing path and UUID, and the
post-verification log line below (the existing `pool_replace_device`
progress messaging) is the meaningful signal.

## Approach

All code changes stay in `cli/src/replace.rs`.

1. Collapse the `else if` branch at `replace.rs:758-776` into an
   unconditional `else` that always calls
   `verify_existing_luks_open_mapper_target`:

   ```rust
   } else {
       // Open-boundary defense-in-depth for the already-open path:
       // re-classify ownership right before `pool_replace_device` so a
       // close+reopen between planning and execution cannot route pool
       // data into a foreign disk. The classifier checks both backing
       // path and UUID, which catches cloned LUKS headers.
       verify_existing_luks_open_mapper_target(
           runner,
           &new_name,
           &new_mn,
           &new_by_id,
           &new_uuid,
           params.backing_path_resolver,
       )?;
   }
   ```

2. Drop the `note: LUKS mapper is already open but device is not yet
   in pool. Completing replace.` stderr line. With the skip gone, the
   distinction it advertised (in-pool vs not-yet-in-pool) no longer
   matters at this seam.

3. Add an execute-level regression test that proves a drifted
   mapper-name pool row no longer bypasses the verifier. This test
   relies on the sibling plan having deleted `check_new_not_in_pool`
   (see Prerequisites):

   - Build a `ReplacePlan` for an `ExistingLuks { mapper_open: true }`
     target.
   - Configure the planning `pool` so `pool.devices` contains a row
     with `mapper == new_mn` and `luks_uuid != new_uuid` (drifted
     mapper-name shape). With the sibling plan landed, no
     mapper-keyed execute-time guard exists; only the plan-time
     `assert_new_uuid_unique` remains, and it passes for
     `luks_uuid != new_uuid`. Execute reaches the branch this plan
     changes.
   - Arrange the live runner so the open mapper for `new_mn` fails
     verification by either reporting a mismatched backing kernel path
     against the resolver's canonicalization of `new_by_id` or by
     reporting a mismatched LUKS UUID. Either failure shape works;
     pick the one that requires the least mock setup. The verifier-level
     tests at `replace.rs:5473` (backing mismatch) and `replace.rs:5642`
     (UUID mismatch) show both runner shapes already in use.
   - Execute the plan and assert:
     - the returned error is the structured verifier-shape failure
       (`ReplaceError::NewTargetMapperBackingMismatch` or
       `ReplaceError::NewTargetUuidMismatchAtOpen`, matching the chosen
       failure shape), not a planning-shape `ReplaceError::Validation`;
     - no `CmdRequest::BtrfsReplaceStart` was issued.

   This test fails against pre-deletion code (the skip would fire on
   the `mapper == new_mn` row, bypass the verifier, and let
   `BtrfsReplaceStart` issue) and passes against post-deletion code.
   It is the test that meaningfully pins the deletion against
   reintroduction.

   Prefer reusing existing scaffolding (the
   `runner_with_active_mapper_uuid` helper and `MockBackingPathResolver`
   used by the verifier-level tests, plus whatever execute-level
   `ReplacePlan` builder is already used for replace execute tests) so
   the new test stays a thin behavioral assertion rather than a
   duplicate mock harness.

## Verification

1. `just test-rust`
2. `just test-vm replace-new-already-luks replace-cloned-luks-header-rejected replace-live-disk`

`replace-new-already-luks` covers the ExistingLuks replacement path, and
`replace-cloned-luks-header-rejected` covers the backing path plus UUID
ownership boundary for an already-open replacement target.

## Out of scope

- Do not change `verify_existing_luks_open_mapper_target` itself; its
  ownership check is already backing-path plus UUID based.
- Do not delete or modify `check_new_not_in_pool`; the sibling plan
  (`plans/wip/replace-check-new-not-in-pool-by-uuid.md`) deletes that
  helper and is a required prerequisite (see Prerequisites), not a
  piece of work to fold in here.
- Do not weaken `assert_new_uuid_unique`; it remains the primary
  pre-plan UUID collision guard and is what makes the always-on
  verifier safe in normal valid replace runs.
