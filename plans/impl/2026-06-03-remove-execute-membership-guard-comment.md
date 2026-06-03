# Plan: clarifying comment for the remove.rs execute-time membership guard

## Context

`braid remove` re-loads pool.json membership inside `RemovePlan::execute`
(`cli/src/remove.rs#RemovePlan::execute`) and re-checks that the target still
exists before building the journal:

```rust
let pre_membership = membership::load_membership(params.paths)?;
if pre_membership.by_uuid(&work_plan.target_uuid).is_none() {
    return Err(absent_from_membership_error(work_plan.name.as_str()));
}
```

A code-review finding flagged this as redundant with the plan-time `by_name`
resolution (`resolve_target_in_membership`) and the two `validate_pool_topology`
calls. Investigation showed it is NOT redundant:

- The two `validate_pool_topology` calls probe *live btrfs state* (`probe_pool`),
  never pool.json -- an orthogonal concern that cannot catch a pool.json-only
  rewrite.
- This fresh `load_membership` is the journal's source of truth: it becomes
  `pre_membership` for `journal::build_journal`, after
  `target_membership.remove_by_uuid(...)`. If the target vanished from pool.json
  during the confirmation prompt or sleep-inhibitor acquire, `remove_by_uuid`
  silently no-ops and the journal records a misleading "removed nothing."
- Git history confirms intent: `ffe21e1c fix(remove): reject pool membership
  drift before journaling` added the guard as a dedicated fix.
- Proven load-bearing by `execute_rejects_when_pool_json_drifts_after_planning`,
  which mocks *unchanged* btrfs topology plus a rewritten pool.json so that only
  this guard fires (asserts the inhibitor was acquired and no journal was
  written).

The rationale currently lives in the `absent_from_membership_error` doc comment
and the test preamble, but nothing sits at the call site. Risk: a future reader
mistakes the guard for belt-and-suspenders and deletes it, silently dropping the
drift defense. This plan adds a localized comment so that can't happen.

## Change

One inline `//` comment in `cli/src/remove.rs#RemovePlan::execute`, inserted
between the `load_membership` and the `by_uuid(...).is_none()` guard. Mirror the
sibling post-journal gate comment in the same function (the "(Post-journal)
last-moment safety gate" block): concise phase label, the window being closed,
the failure mode, and the covering test via the house `pinned by <test_fn>`
convention (already used in `recover.rs` / `replace.rs`).

Proposed text (implementer may tune within convention):

```rust
// (Confirm/inhibitor-window guard) This fresh load is the journal's
// pre_membership below, so re-check the target still exists: a concurrent
// pool.json rewrite during the confirmation prompt or inhibitor acquire
// would otherwise let remove_by_uuid silently no-op and journal a
// misleading "removed nothing." Pinned by
// execute_rejects_when_pool_json_drifts_after_planning.
```

Constraints honored:

- ASCII `--` and straight quotes (writing-style rule).
- No line numbers -- the test is cited by bare greppable fn name (File
  References rule).
- Inline `//` body comment, not a `///` doc comment, so the Doc Comments rule
  for `pub` items does not apply.
- Narrow hand edit; no formatter run (`cargo fmt` is off per project rule).

## Deliberately NOT doing

- **Not touching `absent_from_membership_error`'s doc comment.** It is *shared*
  with the planner, which has no inhibitor/confirmation window. Executor-specific
  window detail belongs at the call site, not the shared constructor.
- **Not modifying `replace.rs`.** Investigation found a real asymmetry:
  `ReplacePlan::execute` journals from a *plan-time* `pre_membership` /
  `target_membership` snapshot (fields on the plan, computed before the
  inhibitor) and has no execute-time membership re-check or pool.json-drift test,
  unlike remove. This *may* be a latent drift gap, or may be acceptable given
  replace's recovery semantics -- unverified. That is a correctness question, not
  a documentation one (see follow-up). We deliberately do NOT add a speculative
  "might be a bug" NOTE to replace.rs: braid comments state proven invariants, so
  an unverified hypothesis in production would recreate the very confusion this
  fix removes.

## Recommended follow-up (separate task)

Investigate whether `ReplacePlan::execute` needs the same
confirm/inhibitor-window membership guard. Trace replace's recovery
reconciliation (`recover.rs`) and the op-lock / mutation-preflight serialization
to decide whether a concurrent pool.json rewrite during replace's window can
produce a lost update or a misleading journal. If real, the fix is to re-load
membership at execute (mirroring remove) plus add an
`execute_rejects_when_pool_json_drifts_after_planning` analog for replace. Likely
higher-value than the comment itself, but behavioral -- keep it out of this docs
change.

## Critical files

- `cli/src/remove.rs` -- the only file changed (`RemovePlan::execute`, the
  `load_membership` + `by_uuid(...).is_none()` block).
- Style reference: the post-journal gate comment in the same function;
  `absent_from_membership_error` (already-documented rationale); test
  `execute_rejects_when_pool_json_drifts_after_planning`.

## Verification

Comment-only change to a function body -- it cannot alter behavior. Confirm it
compiles and the guard's covering test still passes:

- `just test-rust` -- builds `braid-cli` and runs unit tests, including
  `execute_rejects_when_pool_json_drifts_after_planning` (unchanged, must stay
  green).
- Eyeball the comment at the right indentation, reading consistently against the
  sibling post-journal-gate comment.
