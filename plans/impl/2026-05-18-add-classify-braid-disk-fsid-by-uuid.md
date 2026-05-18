# Plan: Make add returned-disk classification UUID-keyed (with backing-path proof)

## Context

`classify_braid_disk_fsid` in `cli/src/add.rs` classifies an already-open
`PresentLuks` add target after proving that:

- the disk has the expected braid LUKS label,
- the open mapper has a btrfs superblock, and
- that btrfs FSID matches the mounted braid pool FSID.

The final branch currently decides whether the disk is already in the
live pool by comparing the expected mapper name:

```rust
if pool.devices.iter().any(|d| d.mapper == *mapper) {
    return Ok(AddLuksIdentity::BraidLabeledAlreadyInPool);
}

Ok(AddLuksIdentity::BraidLabeledRecoverable)
```

That is a UUID-identity violation. The function is not merely checking
a runtime handle collision; it is deciding between "already in pool"
and "recoverable returned disk". Per
`docs/decisions/024-luks-uuid-identity.md`, live pool correlation must
use the target's LUKS UUID, not the reconstructed `braid-<name>` mapper.

A naive fix -- replace the mapper predicate with `d.luks_uuid ==
*target_uuid` -- would weaken clone protection. ADR-024 invariant 8
requires "backing path match first, then UUID match" for any path that
reuses an open mapper; collapsing live-pool membership onto UUID-equality
alone would let a cloned LUKS header (same UUID, different physical
disk) slip past the open-mapper branch as a no-op before
`assert_target_uuid_unique` could reject it. The existing live-pool
collision test at `add.rs` `add_pre_write_uniqueness_assert_live_pool_collision`
(line 8301) depends on that rejection.

## Intended outcome

Two decisions live behind the current `classify_braid_disk_fsid`: the
btrfs probe (does this open mapper carry the pool's btrfs FSID?) and
live-pool identity classification (is this UUID a current pool member,
a clone of one, or a returned disk?). Split them.

After this change:

- The btrfs probe keeps using the mapper path -- that is the right
  handle for `btrfs filesystem show /dev/mapper/<mapper>` and answers
  only "does this mapper belong to the pool's btrfs FSID?".
- A new add-local helper decides live-pool membership by LUKS UUID plus
  canonical backing-path evidence:
  - matching UUID + same canonical backing path -> already-in-pool
    no-op (drift-tolerant: live mapper name may differ from
    `braid-<name>`).
  - matching UUID + different canonical backing path -> clone
    duplicate; refuse with `AddError::DuplicateUuid`.
  - no matching UUID -> not in live pool; treat as recoverable
    candidate.

This satisfies ADR-024 invariant 8 (backing path first, then UUID) and
preserves the clone-rejection contract while tolerating benign mapper
drift in both the open-mapper and closed-mapper PresentLuks branches.

## Approach

All code changes stay in `cli/src/add.rs`. The existing
`BackingPathResolver` trait (`cli/src/luks.rs`) is reused; no new
trait or resolver type is introduced.

1. Narrow `classify_braid_disk_fsid` to btrfs probing only.

   Drop the `pool.devices` membership branch. The function returns a
   smaller result that says only what the btrfs probe can prove:

   ```rust
   enum AddLuksBtrfsProbe {
       NoBtrfs,         // BraidLabeledNoBtrfs
       ForeignPool,     // BraidLabeledForeignPool
       SamePool,        // pool FSID matched; membership is undecided
   }
   ```

   `AddLuksIdentity` is reduced accordingly (its `AlreadyInPool` and
   `Recoverable` variants are no longer produced by the btrfs probe).
   The mapper parameter stays because the btrfs probe still needs it.

2. Add a new add-local helper for live-pool identity classification:

   ```rust
   enum LivePoolMatch<'a> {
       SameBacking { device: &'a PoolDevice },
       DifferentBacking { device: &'a PoolDevice },
       NoMatch,
   }

   fn classify_live_pool_match<'a>(
       target_uuid: &LuksUuid,
       target_by_id: &ByIdPath,
       pool: &'a PoolState,
       resolver: &dyn BackingPathResolver,
   ) -> Result<LivePoolMatch<'a>, AddError>
   ```

   Canonicalize `target_by_id.as_str()` once via the resolver, then
   scan **every** `PoolDevice` whose `luks_uuid == *target_uuid`,
   canonicalizing each `device.underlying` and comparing to the
   target's canonical kernel path. `probe_pool`
   (`cli/src/probe.rs:465`) does not dedupe by `luks_uuid`, so two
   live rows with the same UUID are possible (e.g. both halves of a
   clone happen to be open at probe time); the helper must fail
   closed in that case.

   Precedence -- `DifferentBacking` dominates:

   - If any matching-UUID `PoolDevice` canonicalizes to a different
     kernel path than the target, return
     `DifferentBacking { device }` naming the first such device (so
     the operator-visible error names a concrete live mapper).
   - Else, if at least one matching-UUID `PoolDevice` canonicalizes
     to the same kernel path as the target, return
     `SameBacking { device }` naming that device.
   - Else, return `NoMatch`.

   Canonicalization failure on either side (target or any scanned
   `device.underlying`) is a hard error (`AddError::Validation`) --
   ADR-024 invariant 8 requires the backing-path proof; we do not
   guess.

3. Open `PresentLuks` planning branch (around `add.rs:1855`):

   ```text
   classify_braid_disk_fsid(...)
     NoBtrfs / ForeignPool -> existing identity_to_error path
     SamePool ->
       classify_live_pool_match(uuid, by_id, pool, resolver)
         SameBacking      -> AlreadyInPool no-op (continue)
         DifferentBacking -> AddError::DuplicateUuid {
                                in-flight side = (name, by_id),
                                live side = synth (mapper, by-id) like
                                  assert_target_uuid_unique's (2b) arm
                             }
         NoMatch          -> Recoverable; build RecoverableBraidTarget,
                             then assert_target_uuid_unique as today.
   ```

   The explicit `DuplicateUuid` raise here replaces the path that
   currently flows through `assert_target_uuid_unique` in the
   `Recoverable` arm. The synthesized name/by-id pair matches
   `assert_target_uuid_unique`'s `(2b)` arm so the existing error
   message shape is preserved.

4. Closed `PresentLuks` planning branch (around `add.rs:1900`):

   Call `classify_live_pool_match` before `assert_target_uuid_unique`.

   ```text
   SameBacking      -> AlreadyInPool no-op (continue)
                       -- drift-tolerant skip on the canonical-backing arm
   DifferentBacking -> AddError::DuplicateUuid (same shape as the open
                       branch above; supersedes assert's (2b) arm for
                       this candidate)
   NoMatch          -> fall through to existing assert_target_uuid_unique,
                       which still rejects in-flight and membership
                       collisions.
   ```

   `assert_target_uuid_unique`'s `(2b)` live-pool arm becomes
   defense-in-depth for paths that bypass the new helper (Fresh
   branch, future call sites); leave the function unchanged so the
   in-flight and membership arms continue to fire and the live-pool
   arm remains as the backstop.

5. Closed `PresentLuks` Pass-1 execution branch (around `add.rs:966`):

   Update its `classify_braid_disk_fsid` call to the narrowed
   signature. Live-pool re-classification is not needed here -- the
   planner already gated this target through step 4. Translate
   `SamePool` to the existing `Recoverable` execution path.

6. Plumb `BackingPathResolver` into `AddStepsInput` so
   `build_add_work_plan` (and the test harness) can pass it down to
   the new helper. `AddParams::backing_path_resolver` already exists;
   this is a struct-field forward.

7. Update direct unit-test call sites for `classify_braid_disk_fsid`
   to the narrowed signature. Add the new helper's tests next to them
   (see Regression tests).

## Regression tests

### Unit tests for `classify_live_pool_match`

Add a focused test block near the existing classifier tests covering
the three branches:

- `live_pool_match_same_backing`: pool has one device with the target
  UUID; resolver canonicalizes the candidate by-id and the device's
  `underlying` to the same kernel path. Expect `SameBacking`.
- `live_pool_match_different_backing`: pool has one device with the
  target UUID; resolver canonicalizes them to different kernel paths.
  Expect `DifferentBacking`.
- `live_pool_match_no_uuid`: pool has devices but none with the target
  UUID. Expect `NoMatch`.
- `live_pool_match_canonicalize_error`: resolver fails on one side.
  Expect `AddError::Validation`. This pins the "no guessing" rule.
- `live_pool_match_mixed_same_and_different_backing`: pool has two
  devices with the target UUID. One canonicalizes to the same kernel
  path as the candidate `by_id`; the other to a different path.
  Expect `DifferentBacking` (precedence rule). Pins the fail-closed
  rule against the clone-with-one-half-open scenario.

### Work-plan level regression tests

Add four `build_add_work_plan` tests, one per branch x mapper-state
cell, to cover the production wiring (each test must fail before the
change for a different reason):

1. Open mapper + same-UUID + same canonical backing (drift):
   `pool.devices[0]` has `mapper: "braid-drifted"` but `underlying`
   canonicalizes to the same kernel path as the candidate `by_id`.
   Expect: `AlreadyInPool` no-op (plan succeeds, no
   `CryptsetupLuksFormat`, target not added to journal). Fails today
   because the mapper-equality predicate misses the drifted live row
   and falls through to the recoverable branch, which then trips
   `assert_target_uuid_unique` -> `DuplicateUuid`.

2. Open mapper + same-UUID + different backing (clone):
   `pool.devices[0].underlying` canonicalizes to a different kernel
   path than the candidate `by_id`. Expect:
   `AddError::DuplicateUuid` naming both by-ids. This is a regression
   guard for the existing
   `add_pre_write_uniqueness_assert_live_pool_collision` test --
   keep that test as well; this one pins that the new helper raises
   the same error variant before `assert_target_uuid_unique` would.

3. Closed mapper + same-UUID + same canonical backing (drift):
   Candidate probed as `PresentLuks { mapper_open: false }`.
   `pool.devices[0]` has matching UUID and same canonical backing as
   the candidate `by_id`. Expect: `AlreadyInPool` no-op. Fails today
   because `assert_target_uuid_unique`'s `(2b)` live-pool arm
   rejects the UUID before any drift consideration.

4. Closed mapper + same-UUID + different backing (clone):
   Candidate probed as `PresentLuks { mapper_open: false }` with a
   matching UUID against a foreign mapper in the live pool with a
   different canonical backing. Expect: `AddError::DuplicateUuid`.
   This must still fail closed; the existing
   `add_pre_write_uniqueness_assert_live_pool_collision` exercises
   approximately this case but for the open-mapper input (see the
   `cloned_disk_probed` helper, which builds `mapper_open: true`).
   Add a closed-mapper twin alongside it.

### Updated direct classifier tests

Keep the existing `classify_fsid_already_in_pool`,
`classify_fsid_recoverable`, `classify_fsid_foreign_pool`, and
`classify_fsid_no_btrfs` coverage. Rewrite their expectations against
the narrowed `AddLuksBtrfsProbe` enum:

- `already_in_pool` and `recoverable` collapse into one
  `classify_fsid_same_pool` test (both produced `SamePool` now).
- `foreign_pool` -> expect `ForeignPool`.
- `no_btrfs` -> expect `NoBtrfs`.

The split between "already in pool" and "recoverable" moves to the
new `classify_live_pool_match` tests above.

## Verification

1. `just test-rust`
2. `just test-vm braid-add-disk add-returned-disk-after-remove-missing braid-add-cloned-luks-header-rejected`

The VM checks cover the normal add path, returned-disk adoption, and
duplicate/cloned LUKS UUID protection. `braid-add-cloned-luks-header-rejected`
in particular pins the operator-visible refusal for the
clone-against-live-pool case.

## Out of scope

- Do not change `ensure_luks_open` or `classify_mapper_ownership`;
  those helpers are runtime mapper ownership checks and already
  require backing path plus UUID evidence.
- Do not change `assert_target_uuid_unique`'s in-flight or membership
  arms. Its live-pool `(2b)` arm stays in place as defense-in-depth
  for the Fresh branch and any future call site that doesn't go
  through `classify_live_pool_match` first.
- Do not move `BackingPathResolver` out of `cli/src/luks.rs`; this
  plan only threads it through `AddStepsInput`.
