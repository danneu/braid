# Drop the post-lock-hoist `remove-missing` drift recheck

## Context

`RemoveMissingPlan::execute` reloads `pool.json` and re-resolves the
requested `--missing-id` against fresh membership, then aborts if the
devid now resolves to a different UUID than the planner captured. This
recheck was added in commit `7c15492` on 2026-05-13 (impl plan:
`plans/impl/2026-05-13-remove-missing-dry-run-devid-resolution.md`),
at a time when `/run/braid-pool.lock` was owned by the shell wrapper
and the recheck's plan invoked
`docs/decisions/022-dry-run-preview-model.md` to justify
execution-time validation.

Six days later, commit `ff6f766` (2026-05-19) moved the pool lock
into Rust dispatch, and commit `40c38a9` (2026-05-20) centralized the
policy table. The acquisition in `cli/src/main.rs:492`
(`acquire_per_policy(&pool_lock, lock_policy(&cli.command))`) now
binds `_pool_guard` for the duration of `main()`, which covers
`cmd_remove_missing` end-to-end -- including the planner's
`load_membership` (`remove_missing.rs:443`) and the executor's
re-resolve (`remove_missing.rs:206-220`). The lock policy for
`Commands::RemoveMissing` is `NonBlocking` on the real-run path
(`main.rs:120-126`), so any concurrent braid invocation that would
mutate `pool.json` exits immediately with the "another braid
operation is already in progress" error rather than waiting.

With that invariant, the only state change the recheck can still
catch is a human or external tool rewriting `pool.json` directly
while the lock is held. Principle 12 in `docs/principles.md` and
decision doc `026-pool-lock-rust-owned.md` both treat that scenario
as out of scope. Doc 022's sanction for execution-time validation
covers state changes outside the lock's domain -- passphrases,
closed mappers, kernel state -- not `pool.json`, which is entirely
inside it. The recheck's own test
(`execute_aborts_when_devid_rebinds_between_plan_and_execute`,
`remove_missing.rs:2659-2732`) has to call `save_membership`
directly under the held lock to fire, modelling a state no
production code path can produce.

Outcome: remove the recheck and its dedicated test. Production
behaviour is unchanged. The user-visible never-enriched refusal --
which fires unconditionally inside the planner -- stays pinned by
the existing
`cmd_remove_missing_never_enriched_refusal_returns_structured_error`
and `cmd_remove_missing_never_enriched_refusal_in_dry_run` tests.

## Changes

All edits land in `cli/src/remove_missing.rs`.

1. **Delete the drift recheck** at `cli/src/remove_missing.rs:209-220`
   (the doc comment, the `resolve_removal_target` call, the
   `fresh_uuid != target_uuid` branch). Keep the
   `load_membership(params.paths)` call at line 206 and the
   `pre_membership` binding -- the journal builder
   (`journal::build_journal(pre_membership, ...)`, line 227) and
   `target_membership` (line 224) still need it. Tighten the
   surviving comment around `target_membership` if the now-orphaned
   "uuid was just re-confirmed" reading no longer matches the
   reduced code.

2. **Delete the drift test** at
   `cli/src/remove_missing.rs:2659-2732` -- the
   Intent/Why/Scenario preamble plus the `#[test] fn
   execute_aborts_when_devid_rebinds_between_plan_and_execute`
   body. Nothing else references this symbol.

3. **No change** to `resolve_removal_target` (still used by the
   planner at `remove_missing.rs:453`), the work-plan struct, or
   the never-enriched refusal tests (lines 2507 and 2590). No
   change to `cli/src/main.rs`, the membership module, or any VM
   test.

4. **No doc edits.** The 2026-05-13 impl plan stays as historical
   record; it documents the state of the codebase at the time and
   is not a living spec. Principles 12 and decision 026 already
   state the invariant that makes the recheck redundant.

## Verification

- `just test-rust` -- the rust unit suite. The deleted test
  disappears; the remaining `remove_missing` tests (positive path,
  preflight refusals, never-enriched refusal in both real-run and
  dry-run, post-remove persistence failure) must stay green.
- `cargo build -p braid-cli` via `just test-rust` covers the
  compile-clean check; confirm no `unused_imports` warning for
  `resolve_removal_target` (still referenced in the planner) or
  for any membership helper.
- `just test-vm braid-remove-disk` -- the lifecycle VM check that
  exercises `braid remove-missing` end-to-end (graceful remove,
  dead-disk remove-missing, pool.json pruning, LUKS cleanup). This
  is the production-reachable execute path the deleted recheck
  used to live on. Scope is small; do not run the full
  `just test-vm` unless this check fails in a way that suggests
  broader impact.
- No fixture refresh: no parser-critical tool versions are touched.
