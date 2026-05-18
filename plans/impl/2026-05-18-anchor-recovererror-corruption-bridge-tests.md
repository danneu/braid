# plan: anchor `RecoverError` corruption variants with bridge tests

## Context

A prior review claimed `RecoverError` has many specific variants alongside
the catch-all `Failed(String)` and proposed collapsing structured variants
that no caller pattern-matches into `Failed(String)`. Audit results disagree
with that prescription:

- **`JournalUuidDevidGap`**: dead variant. Removed separately. Not this
  plan's concern.
- **`AckCleanupFailed { stage, detail }`**: already matched. Tests assert
  on `stage` at `cli/src/recover.rs:6453` and `cli/src/recover.rs:6503`; the
  sibling `AddError::AckCleanupFailed` is matched at `cli/src/add.rs:4609`,
  `4663`, `4726`. No change.
- **`DuplicateDevidDuringReplay { devid, members }`** and
  **`NoMemberForJournaledDevid { devid }`**: typed intentionally. The
  internal enum `JournaledSnapshotError` (`cli/src/recover.rs:86-89`) exists
  specifically to keep corruption signals type-distinct from topology
  mismatches; the design intent is documented at
  `cli/src/recover.rs:80-84`. The 14 construction sites (`1706`, `1709`,
  `2747`, `2750`, `2771`, `2774`, `2842`, `2845`, `3158`, `3161`, `3176`,
  `3179`, `3284`, `3287`) bridge `JournaledSnapshotError` into the matching
  `RecoverError::*` variant. The Display text on `NoMemberForJournaledDevid`
  carries operator guidance pointing at `docs/luks-unlock.md` and
  `pending-op.json`; collapsing to `Failed(String)` forces every call site
  to inline that message, multiplying maintenance.
- **`Probe`, `Cmd`, `Parse`, `Membership`, `Mount`, `Luks`**: `#[from]`
  source-bridge variants that support `?` propagation. Collapsing them to
  `Failed(String)` removes the ergonomic. `Mount` is also matched at
  `cli/src/main.rs:884-889` for the exit-code-2 contract; `Luks` is matched
  at `cli/src/recover.rs:8759`.
- **`Journal(String)`**: stringly-typed today; collapsing would erase one
  semantic group but adds no functional value. Out of scope for this plan.

The actual gap is missing bridge-level test coverage for the two corruption
variants. Existing tests
`live_pool_matches_membership_propagates_duplicate_devid_from_null_underlying`
(`cli/src/recover.rs:11073`) and
`live_pool_matches_membership_rejects_null_underlying_without_expected_devid`
(`cli/src/recover.rs:10937`) only assert on the internal
`JournaledSnapshotError`. Nothing pins the bridge contract -- a future
refactor could replace any of the 14
`Err(RecoverError::DuplicateDevidDuringReplay { .. })` arms with
`Err(RecoverError::Failed(...))` and every test would still pass.

**Goal**: keep the current typed inventory as-is; add two small bridge tests
so the corruption-vs-mismatch type distinction is compile-anchored instead
of comment-anchored.

## Change

Add two regression tests inside the existing `#[cfg(test)] mod tests` block
in `cli/src/recover.rs`. Each drives an executor function that calls
`live_pool_matches_membership` with a corrupt journaled snapshot and asserts
the typed `RecoverError::*` variant -- not `RecoverError::Failed`.

The simplest call site is
`execute_remove_missing_pool_mutation_recovery` (corruption checks at
`cli/src/recover.rs:2746-2751` for `pre_membership` and `2770-2775` for
`target_membership`): the `recoverable_pool_mutation_remove_missing_journal`
fixture and surrounding helpers needed to drive it already exist in the test
module. Place the new tests near
`live_add_recovery_ack_cleanup_failure_returns_typed_error_and_preserves_journal`
(`cli/src/recover.rs:6473-6509`), which is the project's reference pattern for
"typed recovery error survives the bridge".

Reuse:

- `PoolMembership::for_corruption_tests()` (`cli/src/membership.rs:395-403`)
  -- bypass normal insert validation when building a `pre_membership` with
  duplicate devids. Already used by `cli/src/recover.rs:11075`,
  `cli/src/lock.rs:3213`, `cli/src/status.rs:3797`.
- `membership_from()` (`cli/src/recover.rs:4659`) and `membership_entry(...)`
  -- standard membership builders used throughout the recover tests.
- `recoverable_pool_mutation_remove_missing_journal()` (or the nearest
  remove-missing journal fixture in the test module) -- mutate the journal's
  `pre_membership` to inject corruption before passing it through.
- `PoolFixture::empty()` plus `f.recover_params().build()` -- standard test
  fixture for recover params; see `cli/src/recover.rs:6473-6509` for the
  shape.

### Test 1: `bridges_duplicate_devid_corruption_to_typed_recover_error`

- Build a `pre_membership` via `PoolMembership::for_corruption_tests()`
  with two members sharing `devid: 2`.
- Build a live pool that reports devid 2 as missing or null-underlying.
- Call `execute_remove_missing_pool_mutation_recovery` with a journal whose
  `pre_membership` is the corrupt one.
- Assert
  `Err(RecoverError::DuplicateDevidDuringReplay { devid: 2, members })` and
  `members.len() == 2`. Reject `RecoverError::Failed`.

### Test 2: `bridges_no_member_for_devid_to_typed_recover_error`

- Build a `pre_membership` via `membership_from()` carrying devids `{1, 2}`
  only.
- Build a live pool with a missing or null-underlying mapper at `devid: 99`.
- Call the same executor.
- Assert `Err(RecoverError::NoMemberForJournaledDevid { devid: 99 })`.
  Reject `RecoverError::Failed`.

Each test uses the standard `// Intent / Why it exists / Scenario`
preamble per `AGENTS.md`. Each test should be roughly the size of the
existing `AckCleanupFailed` parallel test (~35-40 lines).

No production code changes. The two `RecoverError` variants and their 14
construction sites stay exactly as they are.

## Files to modify

- `cli/src/recover.rs` -- add two tests inside the existing test module.

## Out of scope

- Removing `RecoverError::JournalUuidDevidGap` -- handled separately.
- Tightening the dead `Err(e)` arm in `live_pool_matches_membership` at
  `cli/src/recover.rs:1612-1619` -- separate cleanup.
- Collapsing `RecoverError::Journal(String)` into `Failed` -- `Journal` is
  stringly typed already and changing it has no functional win.
- Renaming or restructuring `JournaledSnapshotError`.
- Re-walking the other 12 construction sites with additional bridge tests:
  one test per variant is sufficient since all 14 sites use the mechanically
  identical `match live_pool_matches_membership(..) { Err(Journaled..) => ..
  Err(RecoverError::..) }` bridge.

## Verification

1. `just test-rust` -- both new tests pass; existing tests continue to
   pass. No production code changed, so behavior is unchanged.
2. Read each new test against the construction sites at
   `cli/src/recover.rs:2746-2775` and confirm the test path actually
   traverses the bridge (not just `live_pool_matches_membership` in
   isolation, which is already covered by `cli/src/recover.rs:10937` and
   `cli/src/recover.rs:11073`).
3. `cargo check -p braid-cli` -- compiles. (Skipped clippy because
   `cargo clippy --workspace -- -D warnings` is currently red on unrelated
   lints.)
