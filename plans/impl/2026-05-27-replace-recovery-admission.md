# Pivot Plan: Replace Recovery Admission Membership

## Summary

Fix the production-reachable `braid recover` panic by making recovery use a
phase-aware admission membership instead of always unioning `pre_membership`
and `target_membership`.

The key behavior: `Replace::PostReplaceMaintenance` must admit only
`target_membership`, because btrfs replace legitimately preserves the old
device's `devid` on the new device after commit. Recovery should complete and
clear `pending-op.json`; it should not turn this valid state into a
`RecoverError`.

## Key Changes

- In `cli/src/recover.rs`, replace `union_memberships` with a documented helper
  like `recovery_admission_membership`.
- Rename `RecoverWorkPlan.union` to `admission_membership` to make its purpose
  explicit.
- Helper behavior:
  - `Replace::PostReplaceMaintenance`: return `journal.target_membership.clone()`
    directly.
  - All other phases: merge `pre_membership` plus target-only UUIDs through
    `PoolMembership::insert`.
  - Return `Result<PoolMembership, membership::MembershipError>` instead of
    panicking on unexpected conflicts.
- In `plan_recover`, convert admission-membership construction failures into
  `PlanFailure::with_notes(notes, RecoverError::Membership(e))`.
- The already-mounted dry-run validation must check live UUIDs against the same
  admission membership. For `Replace::PostReplaceMaintenance`, that means
  target-only admission, not pre-plus-target admission.
- Update comments mentioning "union" where they describe recovery admission or
  foreign-live-device checks.
- Update `docs/commands/recover.md` so the safety-check description is
  phase-aware: most recovery phases reject live members outside the selected
  admission membership, and `Replace::PostReplaceMaintenance` admits only the
  committed target membership.
- Do not add a by-id corrupt-journal regression as the primary fix; that was the
  wrong trigger. The production invariant to pin is valid replace
  post-maintenance with inherited `devid`.

## Test Plan

- Keep `replace_journal()` realistic for the initial `Replace::PoolMutation`
  journal:
  - `pre_membership.old.devid = Some(2)`.
  - `target_membership.new.devid = None`.
  - Keep `disk1.devid = Some(1)`.
- Add a dedicated post-maintenance fixture/helper that starts from
  `replace_journal_in_phase(ReplacePhase::PostReplaceMaintenance, ...)` and
  sets `target_membership.new.devid = Some(2)`, matching the post-commit
  enrichment in live replace.
- Update the two explicit `cmd_recover_replace_post_maintenance_*` fixtures so
  their `pre.old` and `target.new` both carry `Some(2)`.
- Add or adjust one focused unit test asserting `plan_recover` succeeds for a
  `Replace::PostReplaceMaintenance` journal whose old and new members share
  `devid 2`.
- Add one non-post-maintenance admission-conflict test:
  - Build a `Replace::PoolMutation` journal with individually valid
    `pre_membership` and `target_membership` snapshots, but make
    `target_membership.new.by_id` equal `pre_membership.old.by_id`.
  - Call `plan_recover` before any mount planning can matter.
  - Assert it returns `PlanFailure` with
    `RecoverError::Membership(MembershipError::Conflict(_))`.
  - Assert `failure.notes` preserves the recovery entry banner.
- Add one already-mounted dry-run `Replace::PostReplaceMaintenance` test where
  the live pool still contains the pre-only old UUID. Assert `plan_recover`
  refuses it as foreign under target-only admission.
- Existing post-maintenance tests should continue to assert that recovery
  resizes the new disk and clears `pending-op.json`.
- Run `just test-rust`.

## Assumptions

- No CLI output, journal schema, or on-disk membership schema changes are needed.
- Documentation updates are limited to `docs/commands/recover.md` and code
  comments so the documented admission rule matches the phase-aware behavior.
- Unexpected non-replace admission conflicts should fail cleanly instead of
  panicking, but the main acceptance case is successful post-commit replace
  recovery.
