# Tighten `OldDevidMismatch` to Match Replace Behavior

## Summary

Fix the stale internal error model around `braid replace --missing-id`. Today
`OldDevidMismatch` claims it can represent a missing persisted `devid`, but
`resolve_replace_source` returns `OldMemberMissingDevid` before the mismatch
branch when `old_member.devid` is `None`. The ideal fix is to make
`OldDevidMismatch` represent only the real mismatch case: supplied
`--missing-id` differs from the non-null persisted devid for `--old`.

## Key Changes

- In `cli/src/replace.rs`, update `ReplaceError::OldDevidMismatch`:
  - Rewrite the doc comment to say it covers only the supplied `--missing-id`
    disagreeing with the old member's non-null persisted `devid`. Drop the
    "either the persisted devid is `None`" half -- that case is now solely
    `OldMemberMissingDevid`.
  - Change `pool_devid: Option<u64>` to `pool_devid: u64`.
  - Rename the `observed: u64` field to `supplied_devid: u64`. At the mismatch
    branch (replace.rs:1710) this value is the operator's `--missing-id`,
    returned before any `pool.missing_devids` cross-check, so it is supplied
    input -- not a devid btrfs reported.
  - Reword the display message so it stops claiming btrfs reported the value and
    renders `pool_devid` plainly (no longer `{pool_devid:?}`):
    `--old '{old_name}' records devid {pool_devid} in pool.json, but --missing-id was {supplied_devid}. --old and --missing-id disagree about which member is being replaced.`

- Update the only constructor:
  - In `resolve_replace_source`, keep the existing early
    `OldMemberMissingDevid` return for `old_member.devid == None`.
  - In the supplied `--missing-id` mismatch branch (replace.rs:1711), pass
    `pool_devid: persisted_devid` (instead of `Some(persisted_devid)`) and
    `supplied_devid: supplied` (the renamed field, previously `observed: supplied`).

- Update the existing mismatch unit test:
  - In `missing_id_disagrees_with_persisted_devid`, destructure the renamed
    field and assert `pool_devid == 2` and `supplied_devid == 99`.
  - Add an observable error-message assertion: the rendered error contains
    `records devid 2` and `--missing-id was 99`, and contains neither `Some(2)`
    nor the stale `btrfs reports missing devid` phrasing.

- Add a sibling unit test for the missing-persisted-devid case with a supplied
  `--missing-id` (`missing_path_without_persisted_devid_rejected_with_missing_id`):
  - Mirror `missing_path_without_persisted_devid_rejected` (`member.devid = None`)
    but pass `missing_id = Some(2)`.
  - Assert the error is `ReplaceError::OldMemberMissingDevid`, pinning the
    Assumptions claim that a supplied `--missing-id` does not rescue a missing
    persisted devid.

- Leave `OldMemberMissingDevid` behavior unchanged:
  - It remains the sole error for missing-path replacement when `pool.json` has
    no recorded devid for `--old`.
  - Do not merge it into `OldDevidMismatch`; the remediation is different.

## Public Interfaces / Types

- No CLI flag or behavior changes.
- No docs change required unless a stale mention is found during the final
  sweep.
- Internal Rust type shape changes: `ReplaceError::OldDevidMismatch.pool_devid`
  becomes `u64` instead of `Option<u64>`.

## Test Plan

- Run focused Rust unit tests for replace behavior:
  - `cargo test -p braid-cli missing_id_disagrees_with_persisted_devid`
  - `cargo test -p braid-cli missing_path_without_persisted_devid_rejected`
    (substring-matches both the existing test and the new
    `..._with_missing_id` sibling)
- Run a final search for stale references (fixed-string, since the patterns
  contain regex metacharacters):

  ```sh
  rg -n -F \
    -e 'pool_devid: Some' \
    -e 'pool_devid, Some' \
    -e 'records devid {pool_devid:?}' \
    -e 'persisted devid is `None`' \
    -e 'btrfs reports missing devid' \
    -e 'observed: supplied' \
    cli/src/replace.rs
  ```

  Expected: no stale matches.
- No VM test is needed because this is an internal error type cleanup plus a
  user-facing wording improvement covered by Rust unit tests.

## Assumptions

- The intended behavior is the current behavior: missing persisted devid fails
  as `OldMemberMissingDevid`, even if `--missing-id` is supplied.
- The better long-term model is stricter typing, not only a comment edit,
  because `Option<u64>` no longer represents any reachable state for
  `OldDevidMismatch`.
