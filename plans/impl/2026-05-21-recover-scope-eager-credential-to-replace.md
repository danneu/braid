# Plan: Scope recover's eager credential resolve to Replace::PoolMutation

## Context

`execute_recover_initial_open` (`cli/src/recover.rs:914-1026`) currently
resolves a passphrase credential eagerly whenever it has an initial
`open_plan`, regardless of journal op or whether the initial mount needs
to open any LUKS device. The justification in the comment at
`cli/src/recover.rs:926-929` only describes the Replace remount cycle:

```rust
// Recover-specific gate: resolve a credential whenever we have an
// initial mount plan. This is eager on purpose -- even if every mapper
// is already open, a replace remount cycle closes every mapper and must
// reopen them with the same credential.
```

But the only action that actually consumes the eagerly-resolved
credential post-mount is `RecoverWorkAction::RemountCycle`, and that
action is only added when `is_replace_pool_mutation(&journal.op)` is
true (see `cli/src/recover.rs:1381-1411`). The `relock_and_remount`
helper itself takes `&OpenCredential` (`cli/src/recover.rs:3458-3466`),
confirming it is the only post-mount mandatory consumer.

For every other op kind (`RemoveMissing::*`, `Replace::PostReplaceMaintenance`,
`Add::PostAddBalanceRaid1`, `Remove::*`, `GenericLivePool`) the
credential is unused. For `Add::PoolMutation` the credential is needed
inside `execute_add_pool_mutation_recovery` only when a target mapper is
closed or replay verification must run -- and that helper already has a
lazy fallback via `recover_passphrase` (`cli/src/recover.rs:2006-2022`)
that preserves the single-passphrase principle.

Effect today: an operator running `braid recover` after a non-Replace
crash where all mappers happen to be open but the pool is not mounted
sees a passphrase prompt that braid never uses. With
`--passphrase-stdin` the same path consumes one line of stdin for
nothing; with `--passphrase-file` it opens the file unnecessarily
(harmless but pointless). This diverges from the `cmd_unlock` UX rule
("no prompt when every mapper is already open", `cli/src/unlock.rs:95-129`).

The fix narrows the eager resolve to Replace::PoolMutation and reuses
the lazy seam already present in the dispatch, matching the unlock
pattern and the documented `resolve_credential` contract that needs
updating in `cli/src/credential.rs:36-47`.

## The change

Modify `execute_recover_initial_open` in `cli/src/recover.rs` so that:

1. The eager resolve at `:930-939` only fires for
   `is_replace_pool_mutation(&plan.journal.op)`. The comment is
   rewritten to name the relock cycle as the sole reason.
2. The `else` arm of the dispatch at `:946-963`
   (`execute_unlock_and_mount` branch) gains an inline
   `state.credential.is_none()` resolve immediately before the
   `expect`, mirroring the resolve sequence in `cmd_unlock`
   (`cli/src/unlock.rs:100-108`).
3. The `expect` message changes from "credential resolved above when
   open_plan is Some" to something like "credential resolved above
   for this branch", since the precondition is now branch-local rather
   than implied by an outer-block invariant.

No other call sites change. `pre_resolved_credential` from
`discover_add_targets_before_mount` (`cli/src/recover.rs:2047-2127`)
continues to satisfy the `is_none()` guard for Add::PoolMutation cases
that needed to open targets before mount; lazy resolution in
`execute_add_pool_mutation_recovery` (`:2425-2440`, `:2453-2467`)
continues to cover later closed-mapper / replay-verify cases.

### Doc updates

- `cli/src/credential.rs:36-47` -- update the `cmd_recover` bullet to
  describe the new rule: "calls this eagerly only for
  `Replace::PoolMutation` (the relock cycle closes every mapper and
  must reopen with the same credential); other op kinds defer to the
  existing seams." Same comment block, no public-signature change.
- `cli/src/recover.rs:926-929` -- rewrite the inline comment to
  describe the Replace-specific scope.

## Tests

Two new `cmd_recover` unit tests in the existing `#[cfg(test)] mod tests`
block of `cli/src/recover.rs`. Both go through `cmd_recover` end-to-end
(not the lower-level `execute_*_recovery` helpers), because the eager
resolve lives in `execute_recover_initial_open` and only `cmd_recover`
exercises it.

### Test 1: non-Replace all-open recovery does not resolve credential

Pins the new no-prompt behavior. Model on
`post_add_recovery_mounts_when_all_mappers_already_open`
(`cli/src/recover.rs:11722`), which already sets up a non-Replace
recovery with both mappers already open and pool unmounted.

- Use a `RemoveMissing::PoolMutation` journal so the post-mount
  completion path has no credential consumer at all (cleaner than
  Add::PoolMutation, which has a lazy seam that could still mask a
  regression).
- Fixture: pool not mounted, every union mapper present in the
  `fs` paths and reported `mapper_open=true` by the LUKS UUID probes,
  so `open_plan` is `Some` with `to_unlock.is_empty()`.
- **Sentinel for "no credential read":** set `passphrase_file` to a
  path that does not exist (matching
  `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`,
  `cli/src/unlock.rs:1532-1602`). If a regression hoists eager
  resolution above the gate, `luks::read_passphrase` opens the bogus
  path and the test fails deterministically with a file-not-found
  error. Do NOT use `ScriptedPassphraseReader` via
  `RecoverParamsBuilder::tty()`: `credential::resolve_credential` ->
  `luks::read_passphrase` hardcodes `&RealTty`
  (`cli/src/luks.rs:261-266`), so the tty seam never observes the
  eager path. The tty seam is only consulted by the lazy
  `recover_passphrase` / `recover_passphrase_for_context` callers
  (`cli/src/recover.rs:2006-2022`, `:2862-2879`), which the new
  RemoveMissing scenario never reaches.

### Test 2: Replace::PoolMutation all-open recovery resolves and remount-cycles

Pins the kept-eager Replace path -- the actual load-bearing case for
the eager resolve. The existing Replace tests in `cli/src/recover.rs`
either call `execute_replace_pool_mutation_recovery` directly with
`credential: None` (`replace_pool_mutation_*_*`) or use
`RemountHarness` with closed mappers so the initial-unlock branch
resolves the credential anyway -- neither covers
`Replace::PoolMutation + open_plan.to_unlock.is_empty()`.

- Combine `post_add_recovery_mounts_when_all_mappers_already_open`'s
  topology (all union mappers already open, pool unmounted) with a
  `Replace::PoolMutation` journal.
- Wrap the `MockRunner` in `RemountHarness::new` seeded with mapper
  paths present and `already_closed = &[]`, so the eager resolve
  fires first, the initial `execute_mount_only` runs, then the
  `RemountCycle` action closes the planned set and reopens via
  `CryptsetupLuksOpen` consuming the eagerly resolved passphrase.
- Provide a working `passphrase_file` (the fixture default) so the
  eager `resolve_credential` succeeds and the remount cycle can
  reopen mappers with the same credential.
- Assertions: `cmd_recover` returns `Ok`, `pool.json` is written with
  the post-replace membership, `pending-op.json` is cleared, and the
  recorded request log shows the expected `CryptsetupClose` +
  `CryptsetupLuksOpen` pair for the closed/reopened mappers
  (matching the patterns the existing `RemountHarness` tests use).

### Existing tests

- `live_add_recovery_prompts_passphrase_once_when_mapper_closed`
  (`cli/src/recover.rs:5789`) continues to pin Add single-prompt
  behavior via the lazy seam; the fix does not change Add behavior.
- `post_add_recovery_mounts_when_all_mappers_already_open`
  (`cli/src/recover.rs:11722`) continues to pass; after the fix it
  no longer relies on the eager resolve being a no-op for Add post-
  balance (it never needed it).
- Existing Replace pool-mutation tests
  (`replace_pool_mutation_committed_finishes_resize_without_replace_start`,
  `replace_pool_mutation_not_committed_restores_pre_membership`,
  `replace_pool_mutation_existing_luks_with_enroll_uuid_mismatch_preserves_journal`)
  call the lower-level helpers and remain unaffected.

## Files modified

- `cli/src/recover.rs` -- gate the eager resolve, add inline lazy
  resolve in the unlock-and-mount arm, rewrite the inline comment,
  add the new unit test.
- `cli/src/credential.rs` -- update the doc comment block on
  `resolve_credential` so the `cmd_recover` bullet reflects the
  Replace-only eager rule.

## Verification

1. `just test-rust` -- runs `cargo test`. Both new tests must pass:
   the RemoveMissing no-prompt test (bogus passphrase_file sentinel)
   and the Replace::PoolMutation all-open remount-cycle test. All
   existing recover tests must continue to pass.
2. Sanity-check the test-1 sentinel by temporarily reverting the
   gate (running the test with the unconditional eager resolve) and
   confirming it fails with a file-not-found error from the bogus
   passphrase path. Revert before committing.
3. `just test-vm braid-recover braid-recover-remove
   recover-add-mixed-batch` -- targeted VM tests that exercise the
   real recover entrypoint with `--passphrase-stdin` and
   `--passphrase-file`. None assert eager-prompt timing directly,
   so they should continue to pass unchanged.
4. No fixture refresh required: this change does not touch any
   parser-critical tool invocation.
