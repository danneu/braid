# Unify the "bootstrap add" check behind a named `Journal` predicate

## Context

`cli/src/recover.rs` has five sites that distinguish two cases of an
add-recovery journal:

- **Bootstrap add** -- `pre_membership` is empty because no pool existed
  before this op; the journal records the very first add.
- **Existing-pool add** -- `pre_membership` snapshots a known-good pool
  from before the mutation.

Today every site re-derives that distinction inline as
`!journal.pre_membership.is_empty()` (often combined with a
`matches!(op, OpKind::Add { .. })` check). The intent has no name, no
single documented home, and the recurring inline form produced the
specific readability defect raised in the verify-issue finding: in
`mount_membership_for_recover` (`recover.rs:3703-3738`) the two
`Add::PoolMutation` arms are non-adjacent -- a guarded arm at `:3708`
plus a positional catch-all at `:3732` whose correctness depends on
implicit match ordering.

Giving the distinction a named predicate on `Journal` dissolves both
problems at once: the implicit ordering goes away in the cited function,
and the same intent gets a documented name at every other site that
already inlines it. No behavior change.

## The change

### 1. Add `Journal::is_bootstrap_add` in `cli/src/journal.rs`

Add a single `impl Journal { ... }` block right after the struct
definition (currently ends at `cli/src/journal.rs:27`). The struct has
no existing inherent impl.

```rust
impl Journal {
    /// True when this journal records the first add into a
    /// previously empty pool. `pre_membership` is empty, so recovery
    /// has no prior pool state to fall back to and must mount the
    /// targets being added instead.
    pub fn is_bootstrap_add(&self) -> bool {
        matches!(self.op, OpKind::Add { .. }) && self.pre_membership.is_empty()
    }
}
```

The doc comment is the single source of truth for what "bootstrap add"
means -- every call site below relies on it.

### 2. Adopt the predicate at the five call sites in `cli/src/recover.rs`

Each site currently inlines the empty-check; switch it to
`journal.is_bootstrap_add()` (or the negation). Two of the sites also
destructure `op` via `if let` / match-arm pattern; the helper is still
the right intent-marker even though the `Add` discriminant is checked
twice -- the cost is a branch on a `Copy`-cheap enum tag, the win is a
named intent.

| Site                                                 | Current shape                                                                                                                                                  | New shape                                                                                                              |
| ---------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `:974` (bootstrap-mount-failed probe)                | `if plan.journal.pre_membership.is_empty() && let MountError::MountFailed(_) = ... && let OpKind::Add { targets, .. } = ...`                                   | replace the empty-check with `plan.journal.is_bootstrap_add()`                                                         |
| `:1112` (alert cleanup after bootstrap recovery)     | `if matches!(&plan.journal.op, OpKind::Add { .. }) && plan.journal.pre_membership.is_empty()`                                                                  | collapse to `if plan.journal.is_bootstrap_add()`                                                                       |
| `:1236` (pre-mount discover gate for existing-pool)  | `if let OpKind::Add { phase: PoolMutation, targets } = ... && !journal.pre_membership.is_empty() && !params.dry_run`                                           | replace the empty-check with `!journal.is_bootstrap_add()`                                                             |
| `:1417` (`RecoverCompletion::AddPoolMutation` guard) | match-arm `Add { phase: PoolMutation, targets } if !journal.pre_membership.is_empty() => { ... }`                                                              | replace the guard with `if !journal.is_bootstrap_add()`                                                                |
| `:3711` (the cited site)                             | two non-adjacent arms: `Add { phase: PoolMutation, .. } if !journal.pre_membership.is_empty() => &journal.pre_membership` and later `Add { ... } => union`     | merge into one arm: `Add { phase: PoolMutation, .. } => if journal.is_bootstrap_add() { union } else { &journal.pre_membership }` |

The `:3711` merge is what eliminates the implicit-ordering invariant
the finding flagged.

### 3. Leave the sibling match at `recover.rs:1413` alone

The `let completion = match` block has a similar guarded-arm +
`Add { .. }` catch-all shape at `:1480`, but the catch-all body is a
distinct `RecoverCompletion::GenericLivePool` variant rather than a
small-expression sibling. Merging there asymmetrically would be uglier,
and the helper change at `:1417` already names the bootstrap intent at
that arm's guard. The catch-all reads correctly once the guard says
"not bootstrap".

## Critical files

- `cli/src/journal.rs` -- new `impl Journal` block with
  `is_bootstrap_add` (single ~5-line addition).
- `cli/src/recover.rs` -- five call-site updates listed above; one of
  them merges two match arms in `mount_membership_for_recover` into one.

No other files are touched. No changes to `journal.rs` types or
serialization. No changes to `OpKind` or `AddPhase`.

## Test coverage

The change is a refactor with no behavior change. The bootstrap path is
exercised end-to-end by existing tests:

- `bootstrap_recovery_clears_acked_stats` (`recover.rs:5648`)
- `bootstrap_recovery_ack_cleanup_failure_returns_typed_error_and_preserves_journal` (`recover.rs:6457`)
- bootstrap fixtures via `bootstrap_pool_mutation_add_journal()` (`recover.rs:4711`) and `bootstrap_journal()` (`recover.rs:13000`)

The existing-pool path is exercised by the rest of the recover test
suite that uses non-empty pre_membership fixtures. No new tests are
needed: the predicate is a pure delegation to existing field state, and
both bootstrap and existing-pool branches of every touched site are
covered today.

## Verification

1. `just test-rust` -- exercises the recover unit tests including the
   bootstrap cases above. Must pass.
2. `cargo build -p braid-cli` (implied by `just test-rust`) -- catches
   the let-pattern + guard sites at `:1236` and `:1417` if the
   negation shape is wrong.
3. `just test-vm recover-add-mixed-batch add-returned-disk-after-remove-missing` --
   the two VM tests that exercise existing-pool add recovery; confirms
   the recovery path that depends on `mount_membership_for_recover`
   still routes through `pre_membership`.

No fixture refresh and no doc updates required -- this is internal to
the CLI, no user-visible surface changes.
