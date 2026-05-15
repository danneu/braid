# Pin the partial-multi-add journal contract

## Context

A Low-severity finding flagged that no test pins what `pending-op.json`
carries after a mid-loop `btrfs device add` failure inside a multi-target
live-pool `cmd_add`. The existing test
`cmd_add_partial_multi_add_cleans_succeeded_disk_before_later_failure`
(`cli/src/add.rs:4416`) exercises that exact scenario but only asserts
acked-stats hygiene -- it never inspects the journal that `braid recover`
will subsequently replay.

The finding's stated risk: a future refactor that pruned the journal
mid-loop to "match recovery to live state" would silently change
recovery behavior with no failing test. That risk is real; the journal
contract for partial multi-add is currently inferred from PoolMutation
semantics alone (ADR-017 documents the rule, no test pins the
producer-side artifact).

The original finding asked for a NixOS VM test. That is the wrong shape:

- The producer-side journal contract is unit-testable using the exact
  destructure-`OpKind::Add { phase, targets }` idiom already in use at
  `cli/src/add.rs:4806-4818`. The existing partial-multi-add unit test
  at `cli/src/add.rs:4416` already drives `cmd_add` with
  `AddFullPathRunner::live().with_second_add_failure()` against a real
  state directory, so `journal::load_journal(&paths)` works without VM
  scaffolding.
- The consumer-side iteration contract (recover replays only the
  not-yet-live targets and sweeps acked-stats for both) is already
  pinned by `live_add_recovery_drops_ghosts_for_mixed_batch`
  (`cli/src/recover.rs:6078`). The replay loop at
  `cli/src/recover.rs:2504` iterates targets uniformly regardless of
  `AddJournalMode`.
- A round-trip `cmd_add` -> `cmd_recover` test in one unit fights the
  grain: zero existing precedent in the recover suite (which always
  starts from a fixture-built journal), and `AddFullPathRunner`'s
  `fail_second_add` flag is sticky with no reset path.

Outcome: one focused producer-side unit test that catches the
hypothetical journal-pruning refactor.

## Approach

Add one new `#[test]` in `cli/src/add.rs` as a sibling to
`cmd_add_partial_multi_add_cleans_succeeded_disk_before_later_failure`,
with its own preamble pinned to a single intent: the journal-write
contract for partial multi-target Add.

Reuse the existing scaffolding:

- `add_test_setup()` -- `cli/src/add.rs:3646` -- builds the state
  directory, config path, passphrase file.
- `AddMockFs` -- as used at `cli/src/add.rs:4421-4424` -- presents the
  two by-id paths.
- `AddFullPathRunner::live().with_second_add_failure()` --
  `cli/src/add.rs:3783` and `:4074` -- forces the second
  `BtrfsDeviceAdd` to fail.
- `RecordingInhibitor::new()` -- as used at `cli/src/add.rs:4426`.
- `journal::load_journal(&paths)` + destructure
  `OpKind::Add { phase, targets }` -- the exact idiom at
  `cli/src/add.rs:4806-4818`.

Assertions to add (in order):

1. `result.is_err()` -- second add must fail.
2. `runner.added_mappers() == vec!["braid-disk2"]` -- only the first add
   committed.
3. Journal exists: `journal::load_journal(&paths).unwrap().is_some()`.
4. Phase: `journal.op` destructures to
   `OpKind::Add { phase: AddPhase::PoolMutation, targets }`.
5. Targets map has both new disks: collect
   `targets.iter().map(|(_, t)| t.name.clone())` into a `BTreeSet` and
   assert it equals `{disk2, disk3}`. This is the key invariant -- the
   hypothetical "prune to failed only" refactor would shrink this to
   `{disk3}`. (`LuksUuidMap` exposes `iter()` and `keys()` only -- not
   `values()` -- per `cli/src/membership.rs:122-130`.)
6. `pre_membership` is the pool snapshot from before the add:
   `BTreeSet` of `journal.pre_membership.iter().map(|(_, m)|
   m.name.clone())` equals `{disk1}`. `add_test_setup()` seeds disk1
   into membership at `cli/src/add.rs:3659-3669`, so this pins that
   the pre-snapshot is the right shape for recovery to mount from.
7. `target_membership` is the full post-mutation pool snapshot
   (existing members PLUS the new targets, per ADR-017 line 41 and
   the build path at `cli/src/add.rs:1049-1061` which clones
   `self.pool_membership` and inserts each new target): `BTreeSet`
   of names from `journal.target_membership.iter()` equals
   `{disk1, disk2, disk3}`. This is the second axis of the contract --
   recovery uses `journal.target_membership` to rebuild `pool.json`,
   so dropping disk1 here would erase a healthy member.
8. Per-target by-id is preserved (sanity check that the journal is
   recover-replayable -- recovery needs the by-id path to locate the
   disk):
   ```rust
   let disk3 = targets
       .iter()
       .find(|(_, t)| t.name.as_str() == "disk3")
       .map(|(_, t)| t)
       .expect("disk3 target");
   assert_eq!(disk3.by_id.as_str(), "/dev/disk/by-id/virtio-disk3");
   ```

Skip asserting on `target.mode` shape -- both targets are `FreshLuks`
in this scenario, but the consumer-side codepath at
`cli/src/recover.rs:2504` is mode-agnostic and the existing fresh-luks
single-target recovery tests already pin that arm. Pinning mode here
would couple the producer test to an unrelated consumer detail.

The new test does NOT need to re-assert acked-stats state -- the
sibling test at `cli/src/add.rs:4416` already pins that. Each test
keeps its own intent.

### Preamble (literal, will live above the new test)

Use the documented `//` line-comment form per `docs/testing.md:11-22`,
not the `/* ... */` block form the immediate sibling at
`cli/src/add.rs:4403-4414` happens to use. The doc is authoritative;
the sibling is drift to be left alone here, not propagated.

```rust
// Intent: a partial multi-add leaves pending-op.json populated with
//   every originally-requested target so `braid recover` can finish
//   the work the loop interrupted.
//
// Why it exists: ADR-017 makes target_membership a write-once,
//   before-the-irreversible-loop snapshot; the live-pool add loop at
//   cli/src/add.rs:1273-1297 never touches the journal mid-loop. A
//   future refactor that pruned journaled targets to "match recovery
//   to live state" on partial failure would silently change what
//   recover replays without breaking any existing test.
//
// Scenario: cmd_add disk2,disk3 against a mounted pool seeded with
//   disk1 by add_test_setup, with disk3's btrfs device add forced to
//   fail. Assert pending-op.json carries OpKind::Add {
//   phase: PoolMutation, targets: {disk2, disk3} } with
//   pre_membership = {disk1}, target_membership = {disk1, disk2, disk3},
//   and per-target by-id paths intact.
```

### Files

- `cli/src/add.rs` -- single new `#[test]` function inserted
  immediately after
  `cmd_add_partial_multi_add_cleans_succeeded_disk_before_later_failure`
  (after `cli/src/add.rs:4464`). No edits to other tests, no edits to
  production code.

### Suggested function name

`cmd_add_partial_multi_add_journal_carries_all_targets`

Mirrors the sibling's name shape (`cmd_add_partial_multi_add_*`) so the
two pinned aspects of the same scenario (acked-stats vs. journal) read
as a pair.

### Anti-requirements

- Do NOT extend the existing acked-stats test in place (dilutes its
  intent).
- Do NOT add a NixOS VM test (overkill -- the contract is structural,
  not integration-shaped, and project test-speed is an active concern
  per `findings-speed-up-tests.md`).
- Do NOT add a fresh-luks two-target recover unit test (consumer side
  is already covered by `live_add_recovery_drops_ghosts_for_mixed_batch`
  which exercises the same uniform iteration loop).
- Do NOT touch production code in `cli/src/add.rs` -- this is
  purely a test addition that pins behavior the code already has.

## Verification

- `just test-rust` -- new test must pass on first run (the contract
  it pins is already implemented). If it fails, the journal-write path
  has drifted from ADR-017 and that's a real bug, not a test problem.
- Manual sanity (optional): comment out the journal write at
  `cli/src/add.rs:1070-1071`, re-run the new test, confirm it fails
  at "journal must survive partial failure". Restore. This proves the
  test catches the regression class the finding warned about.
- `just test-rust` second run with the new test name as a filter:
  `cargo test -p braid-cli cmd_add_partial_multi_add_journal_carries_all_targets`
  to confirm the test runs in isolation and the assertions trigger as
  designed.

No VM tests, no fixture refresh, no parser canary -- this change is
inert with respect to all integration surfaces.
