# Plan: preserve non-target missing members in phased recovery

## Context

`cli/src/recover.rs:1622-1654` -- `recover_membership_matching_expected` --
rebuilds `pool.json` by walking `pool.devices` only. Its four call sites
(phased RemoveMissing PoolMutation completion at `:2674`, RemoveMissing
PostMaintenance at `:2746`, Replace PoolMutation committed branch at
`:3071`, Replace PostMaintenance at `:3180`) are all guarded by
`live_pool_matches_membership`, which resolves `pool.missing_devids`
through `target_membership.by_devid()` to decide whether the live pool
satisfies the expected union. The gate accepts; the helper then
silently drops every member that the gate covered via the
missing-devid path.

The analogous bug for `OpKind::Remove` was already fixed at
`recover.rs:1080-1108` (commit `41f0462`, "broaden recover guard"). That
fix walks `pre_membership` and re-inserts any disk whose devid still
appears in `pool.missing_devids` or `pool.null_underlying`. The same
broadening was not applied to the phased RemoveMissing/Replace paths
introduced in commit `e14ee8f`.

Concrete failure scenario, 3-disk pool with `disk1`/`disk2`/`disk3` and
`braid remove-missing devid=2`:
- Pool mutation commits (disk2 gone).
- Between then and recovery, disk3 flaps -- btrfs marks it MISSING.
- `live_pool_matches_membership(pool, target_membership={disk1, disk3})`
  resolves missing devid 3 through `target_membership.by_devid` and
  returns `Ok(true)`.
- `recover_membership_matching_expected` iterates only `pool.devices =
  [disk1]`, returns `recovered = {disk1}`.
- `pool.json` is written without disk3; the journal is cleared.
  `braid status` can no longer correlate the still-MISSING btrfs devid
  back to a member; the operator loses identity for the missing disk.

Outcome of this plan: `recover_membership_matching_expected` honors
principle 2/5 -- "btrfs `devid` is the only authorized live-fallback
binding key" when the LUKS UUID is unobservable -- on the surface the
gate currently exposes by re-inserting any expected member whose devid
appears in `pool.missing_devids`. Two regression tests pin the
behavior.

## Approach

Surgical fix inside the helper, not at each call site. The helper
already takes `expected: &PoolMembership` (= `target_membership` at every
call site) which is exactly the membership the gate used to accept
those missing devids. The Remove call site keeps its own external loop
because it uses `pre_membership` -- semantically distinct (the disk
being removed is in pre but not target) and out of scope here.

Scope is narrowed to `pool.missing_devids` only. `pool.null_underlying`
is **not** included in this fix: today's `live_pool_matches_membership`
(`:1586-1620`) ignores `null_underlying` when comparing live∪missing
against expected, so a stray null-underlying device whose devid isn't
in `expected` currently passes the gate (the gate sees its UUID in
neither live nor missing and just compares the remaining sets). A
fallback loop here that walks `pool.null_underlying` and errors on an
unresolved devid would regress that currently-accepted replay path.
Broadening to `null_underlying` is coupled to broadening the gate
itself and belongs in a separate change with its own behavioral test.

## Changes

### Code: `cli/src/recover.rs`

In `recover_membership_matching_expected` (`:1622-1654`), after the
existing `for dev in &pool.devices` loop and before `Ok(recovered)`,
add a fallback loop over `pool.missing_devids`:

- For each devid in `pool.missing_devids`, call
  `expected.by_devid(devid)`:
  - `Ok(Some((uuid, expected_member)))`: skip if `recovered.by_uuid(uuid)`
    is already present (defensive; not reachable under the gate). Else
    build the inserted member from `expected_member` but compute
    `added_at` with the **same precedence the live-device loop uses at
    `:1638-1642`**: `prior.by_uuid(uuid).and_then(|m| m.added_at)` ->
    `expected_member.added_at` -> `Some(now_iso())`. Decision 017
    pins this -- "When rebuilding `pool.json`, recover preserves each
    member's `added_at` from the current `pool.json` if present, else
    from the journal's pre/target membership snapshot; only members
    with no prior timestamp get a fresh `now_iso()` stamp"
    (`017-runtime-disk-membership.md:65`; reinforced at `:30`). Insert
    using struct-update syntax: `DiskMember { added_at,
    ..expected_member.clone() }`.
  - `Ok(None)`: return `RecoverError::NoMemberForJournaledDevid { devid }`.
    The `live_pool_matches_membership` gate at `:1586-1620` resolves
    every missing devid through the same `expected` and would already
    have rejected Ok(false); an Ok(None) here implies the gate's
    invariant broke between the gate's call and ours, which is a hard
    error.
  - `Err(MembershipError::DuplicateDevid { devid, members })`: return
    `RecoverError::DuplicateDevidDuringReplay { devid, members }` to
    match the error vocabulary already used by the four call sites
    (e.g. `:2642`, `:2666`).

Above the new loop, add a normal `//` implementation comment (not
`///`; this is an internal block, and AGENTS.md scopes `///` to new
top-level / `pub`/`pub(crate)` Rust boundaries) explaining the
devid-only binding invariant. Suggested wording: "Re-insert any
expected member whose live binding is devid-only. Per principle 2/5,
btrfs devid is the authorized fallback when LUKS UUID is unobservable.
The live_pool_matches_membership gate has already proven every
pool.missing_devids devid resolves uniquely through expected; this
loop materializes that resolution in the rebuilt membership.
pool.null_underlying is intentionally not consulted -- the gate
doesn't gate on it today, and broadening this loop without broadening
the gate would regress currently-accepted replays."

No changes to call sites. No changes to `live_pool_matches_membership`,
`build_membership_from_live_pool`, or the Remove guard at `:1080-1108`.

### Tests: `cli/src/recover.rs`

Two regression tests. Both reuse existing fixtures.

**Test 1 -- focused unit test on the helper.** Add adjacent to
`recover_membership_matching_expected_rejects_foreign_live_uuid` at
`:10585`. Construct:
- A `PoolState` with `pool.devices = [disk1, disk2]` and
  `missing_devids = [3]`, where disk1/disk2 are live with their LUKS
  UUIDs and disk3 is the "still-MISSING non-target".
- An `expected` membership `{disk1, disk2, disk3}` where each member
  has its `devid` persisted (1, 2, 3) and disk3 carries an
  `added_at` distinct from its `prior` value (e.g. expected =
  `"2026-04-01..."`, prior = `"2026-01-01..."`).
- A `prior` membership matching `expected` but with disk3's
  `added_at` set to the prior timestamp.
- Call `recover_membership_matching_expected(&pool, &expected,
  Some(&prior), &resolver)`.
- Assertions, all behavioral and structure-insensitive:
  - Result contains all three UUIDs.
  - disk3's entry has `name`, `by_id`, and `devid` matching the
    expected member.
  - **disk3's `added_at` equals the prior timestamp, not the
    expected/journal timestamp.** This pins the Decision 017
    precedence invariant against future regression.

Reuse: `resolver_for` (`:3808`), `membership_from`/`membership_entry`
(`:4541`, `:4551`). May need a new tiny fixture builder for "two disks
present, one missing devid" if `pool_state_disk1_with_missing_devid2`
(`:5252`) doesn't cover this shape exactly -- inline the PoolState
build in the test if so.

Preamble (per repo "Test Conventions"):
- Intent: helper re-inserts members whose live binding is devid-only,
  with the same `added_at` precedence as the live-device path.
- Why it exists: principle 2/5 ("devid is the authorized fallback
  binding"); Decision 017 (`added_at` is preserved across all writes,
  `pool.json` first then journal snapshot); the OpKind::Remove guard
  at `:1080-1108` does devid-fallback externally for its path, and
  this test pins the helper-internal equivalent for the four phased
  call sites.
- Scenario: phased remove-missing or replace recovery probes the live
  pool and finds an unrelated disk has flapped to MISSING; recovery
  must keep that disk in pool.json with its original `added_at`
  intact so the operator can still address it and the historical
  timestamp is not lost.

**Test 2 -- end-to-end through `cmd_recover` on the RemoveMissing
PoolMutation completion path.** Mirror
`cmd_recover_remove_preserves_non_target_missing_disk` (`:14463-14535`),
but with a `RemoveMissing { phase: PoolMutation, devid: 2, ... }`
journal instead of `Remove`. Pool topology: disk1 present, disk2 gone
(devid 2 successfully removed by btrfs), disk3 reported MISSING (devid
3 in `pool.missing_devids`). Target membership = `{disk1, disk3}`.
Assert `recover` succeeds and the resulting `pool.json` contains both
disk1 and disk3.

Reuse: the entire scaffolding from the existing Remove test -- `MockFs`,
`MockRunner` with `BtrfsFilesystemShow` + `CryptsetupStatus` +
`CryptsetupLuksUuid` outputs, `resolver_for`, `PoolFixture`,
`recover_params`. The journal fixture is the only new helper, named to
match the existing `remove_3to2_journal_with_devids` (`:14428`):
something like `remove_missing_3to2_journal_pool_mutation_with_devids`.

One `cmd_recover` test is sufficient because the helper's behavior is
the same across all four call sites; the unit test covers the
function, this test covers integration through the dispatcher and
phase advancement. Adding a parallel test for Replace's PoolMutation
committed branch is not needed -- it would exercise the same helper
through more scaffolding.

## Out of scope (do not include in this fix)

- Broadening `live_pool_matches_membership` to consult
  `pool.null_underlying`, and the matching `null_underlying` arm in
  this helper. These are a single coupled change: the gate would need
  to define how a stray null-underlying device (devid not in expected)
  affects accept/reject, and a behavioral test would pin both halves.
  This plan's fallback loop intentionally tracks only the surface the
  current gate exposes.
- Unifying `build_membership_from_live_pool` (`:1928-1962`) and
  `recover_membership_matching_expected` into a single helper. They are
  structurally similar twins, but the Remove path's external
  `pre_membership` re-insertion at `:1080-1108` and the phased paths'
  internal `target_membership` re-insertion described above use
  different snapshots for legitimate reasons. Unification is a refactor
  whose net win is unclear and not load-bearing for the bug.

## Verification

1. `just test-rust` -- runs the two new tests and the full Rust unit
   suite. New tests must pass; nothing existing should regress.
2. `just test-vm recover-remove-missing-completed
   recover-replace-completed` -- existing VM tests for these recovery
   paths must still pass (they exercise the happy / target-still-
   missing branches and should be unaffected).
3. Manual read of the diff against `recover.rs:1080-1108` to confirm
   the new fallback loop mirrors that broadened Remove guard's intent
   under the different (target_membership) source of truth.

## Critical files

- `cli/src/recover.rs` -- helper at `:1622-1654`, four call sites at
  `:2674`/`:2746`/`:3071`/`:3180`, sibling Remove guard at
  `:1080-1108`, gate at `:1586-1620`, existing tests at `:10585` and
  `:14463`.
- `cli/src/membership.rs:284` -- `PoolMembership::by_devid`, the lookup
  the new code uses.
- `docs/principles.md` -- principles 2 (`:17`) and 5 (`:40`) name
  `devid` as the authorized fallback binding when LUKS UUID is
  unobservable. The fix is the in-recovery realization of those
  principles for the phased paths.
