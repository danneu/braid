# Plan: correct the bootstrap-add soft-balance-replay rationale

## Context

`braid recover`'s dispatch maps an interrupted `Add` journal to
`RecoverCompletion::GenericLivePool { replay_raid1_maintenance: true }`
(`cli/src/recover.rs`, the `journal::OpKind::Add { .. }` fallthrough). Its inline
comment justifies the owed RAID1 soft balance as "recover-side replay avoids
stranding the operator with single-profile chunks they have to fix manually."

That rationale is wrong for the only case that reaches this arm. The match routes
live `Add` (`PoolMutation` + `!is_bootstrap_add()`) to `AddPoolMutation` and
`PostAddBalanceRaid1` to `AddPostBalance`, so the fallthrough catches *only*
bootstrap add. A 2+-disk bootstrap runs `mkfs.btrfs -d raid1 -m raid1`
(`cli/src/cmd.rs`, `MkfsBtrfsRaid1`), creating the pool as full RAID1 -- so the
soft replay (`pool_balance_raid1_soft`, idempotent `,soft` convert) normally has
nothing to convert and is an expected near-no-op. The genuine "leftover single chunks" rescue the
comment describes belongs to the live post-add path
`execute_add_post_balance_recovery`, not here.

This is a **docs/comment-accuracy fix only -- zero behavior change.** The flag, the
dispatch, and the `pool.devices.len() >= 2` guard in `replay_owed_raid1_maintenance`
are all unchanged. The flag stays: it is the deliberate typed discriminator between
the two `GenericLivePool` producers (bootstrap add -> replay, remove -> no replay),
introduced by `d8828ee0` to remove a runtime per-op match; collapsing it would add
code, not remove it. The conditional premise from the original finding ("if the
un-inhibited balance is fixed") is already satisfied -- commits `f98f13f3` /
`f52914fb` added and extended the sleep inhibitor now visible at the consume site.

The same inaccurate "single chunks for bootstrap" framing appears in three places.
The fix aligns all three so the whole class of reviewer confusion is dissolved.

## Changes

### 1. Code comment -- bootstrap-add arm (primary)

`cli/src/recover.rs`, the `journal::OpKind::Add { .. } => RecoverCompletion::GenericLivePool`
arm (currently ~lines 1551-1557).

Replace the "avoids stranding the operator with single-profile chunks" comment with
one that states: this arm is reached **only** by bootstrap add (name the two sibling
routes that take the live/post-add cases); mkfs already made the pool full RAID1 so
the soft pass is an idempotent near-no-op; it is kept as cheap defense-in-depth, the
same way braid runs the soft balance elsewhere without pre-checking; do not assume
mkfs leaves zero convertible chunks and do not drop it. Draft:

```rust
journal::OpKind::Add { .. } => RecoverCompletion::GenericLivePool {
    // Reached only by bootstrap add -- live PoolMutation routes to
    // AddPoolMutation and PostAddBalanceRaid1 to AddPostBalance, so the
    // leftover-single-chunk conversion those paths own never applies here.
    // A 2+-disk bootstrap already ran `mkfs.btrfs -d raid1 -m raid1`, so this
    // soft replay is an idempotent near-no-op kept as cheap defense-in-depth;
    // do not assume mkfs leaves zero convertible chunks, and do not drop it.
    replay_raid1_maintenance: true,
},
```

### 2. Doc prose -- `balance-soft.md` "Recover replay"

`docs/internals/btrfs/balance-soft.md`, the "Recover replay" section sentence that
begins "This replay fires for an interrupted `add` ... so the operator is not left
with `single` chunks".

The `add` bucket actually covers two recover paths (both labelled `"add"`): the
interrupted post-add balance (`execute_add_post_balance_recovery`, where the
single-chunk rescue is real) and bootstrap add (near-no-op). Reword to distinguish
them instead of attaching the single-chunk claim to both. Draft replacement:

> This replay fires for an interrupted `add` when the balance state is idle, and
> for the idle/no-paused owed post-maintenance step of `remove-missing` and
> `replace`. The `add` case covers two shapes: an interrupted post-add balance --
> a live add whose convert left `single` chunks behind, where the new disk is
> already in the pool so re-running `braid add` would refuse and recover finishes
> the job -- and a bootstrap add, where `mkfs.btrfs -d raid1 -m raid1` already
> created the pool as full RAID1, so the soft pass normally has nothing to convert
> (an expected near-no-op) but is still run as defense-in-depth.

Keep the hedge consistent with the code-comment draft in change 1: state the
bootstrap pass as an *expected* near-no-op, not an absolute "converts nothing" --
braid does not assume mkfs leaves zero convertible chunks and does not drop the
replay. Prose-only; no headings or links change, so `mdbook-linkcheck2` stays green.

### 3. Test preamble -- regression-stakes wording

`cli/src/recover.rs`, the `// Intent / Why it exists / Scenario` preamble of
`cmd_recover_bootstrap_add_replays_owed_raid1_maintenance` (currently ~17451-17471).

Its "Why it exists" claims a regressed flag "would ... leave the operator with
single-profile chunks." For bootstrap that consequence is inaccurate (mkfs already
wrote RAID1). Keep the test unchanged; fix only the stakes clause to state the real
contract: the idempotent soft pass must keep firing for `Add` so the defensive
balance is never silently dropped (and a real convertible chunk from a future
mkfs/profile change is never skipped) -- the pass itself is a near-no-op after a
RAID1 bootstrap. No test-body/assertion change.

## Out of scope -- leave unchanged

- The flag, the dispatch arms, the `>= 2` guard, the inhibitor -- no behavior change.
- Neutral bootstrap-add preambles already using "owed RAID1 soft balance" framing
  (`recover.rs` tests `bootstrap_recovery_holds_inhibitor_across_balance`,
  `generic_recovery_remove_arm_does_not_acquire_inhibitor`,
  `bootstrap_recovery_inhibitor_failure_aborts_before_balance`,
  `bootstrap_add_inhibitor_failure_stops_before_balance_and_preserves_journal`).
- `recover.rs` `replace_post_maintenance_runs_owed_balance` preamble -- correctly
  applies the single-chunk framing to the missing-device replace path.
- ADR-019 inhibit-sleep -- already accurate; no `## See`/citation links touch the
  edited comment or flag, so no citation/linkcheck fallout.

## Verification

- `just test-rust` -- all three edits are comment/prose only, so existing tests
  (including `cmd_recover_bootstrap_add_replays_owed_raid1_maintenance`) pass
  unchanged; this confirms no accidental logic edit.
- `just docs-build` -- builds the mdBook and runs `mdbook-linkcheck2`; confirms the
  `balance-soft.md` prose edit breaks no links.
- `python3 scripts/docs/check-output-ascii.py` -- comments are exempt, but the
  drafts are ASCII; run to confirm nothing regresses.
- `git grep -n "single-profile chunks\|single\` chunks" cli/src/recover.rs docs/internals/btrfs/balance-soft.md`
  -- confirm every remaining hit is a path where degraded operation genuinely
  produces single chunks (replace/remove-missing/live post-add), not bootstrap.
