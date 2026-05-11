# Remove Duplicate Replace Journal By-Id

## Summary

Remove the top-level `new_by_id` field from `OpKind::Replace`. Keep
`ReplaceJournalTarget.by_id` as the operation replay target path, and keep
`target_membership` as the committed membership snapshot.

## Key Changes

- Update `cli/src/journal.rs` so `OpKind::Replace` contains `phase`,
  `old_name`, `new_name`, `new_target`, `source`, and
  `restore_raid1_after_commit`, but no top-level `new_by_id`.
- Update replace journal writes and phase rewrites to stop passing or
  preserving the removed field.
- Update Rust test constructors and handcrafted VM `pending-op.json` fixtures
  for replace recovery to omit top-level `"new_by_id"`.
- Do not remove runtime `new_by_id` variables in `replace.rs`; they are still
  the parsed CLI input and work-plan data.
- Do not remove `ReplaceJournalTarget.by_id`; recovery uses it for uncommitted
  replace target probing, formatting/enrollment, opening, and header backup.
- Do not update historical `plans/` references unless they are already being
  actively revised; this is not a behavior or user-doc change.

## Test Plan

- Run `just test-rust` to catch all Rust constructor, serde, and recovery unit
  test updates.
- Run targeted VM tests that inject replace journals:
  `just test-vm recover-replace-not-started recover-replace-completed recover-replace-existing-luks-enroll recover-replace-existing-luks-uuid-mismatch`.
- Add or adjust a journal unit test so serialized `OpKind::Replace` output no
  longer contains top-level `"new_by_id"` while still containing
  `new_target.by_id`.

## Assumptions

- No migration or compatibility shim is needed for old pending journals.
- Serde strictness policy is unchanged; this fix removes what braid writes and
  what tests model, but does not introduce `deny_unknown_fields`.
- CLI behavior, recovery behavior, and target prep semantics remain unchanged.
