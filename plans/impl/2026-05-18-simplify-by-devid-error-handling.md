# Plan: simplify `by_devid` error handling in `recover.rs`

## Context

Two `match` arms in `cli/src/recover.rs` handle `PoolMembership::by_devid`
returning an `Err(MembershipError)` variant that the function never
actually constructs. `by_devid` (`cli/src/membership.rs:284-302`) is
typed `Result<Option<...>, MembershipError>` for full-enum
exhaustiveness, but the only error it builds is `DuplicateDevid`. The
non-`DuplicateDevid` arms in `recover.rs` are therefore dead code.

There are three different idioms for this same situation across the
codebase:

1. **`recover.rs:1620-1627`** -- the offender flagged in the finding.
   Catches `Err(e)` and routes to `JournaledSnapshotError::NoMemberForDevid`
   via a no-op inner match: `match e { DuplicateDevid { devid, .. } =>
   devid, _ => devid }`. Both inner arms return the same captured
   `devid` -- pure scaffolding from the LUKS-UUID identity migration
   (commit `9c23a15`).
2. **`recover.rs:1711-1713`** -- silently forwards: `Err(err) =>
   return Err(RecoverError::Membership(err))`. Reachable in the type
   system, unreachable at runtime. Hides the invariant.
3. **`lock.rs:855-869`** -- exhaustive enumeration with a descriptive
   `unreachable!`. Clean and honest about the invariant.

This plan converts both sites in `recover.rs` to the `lock.rs:855-869`
idiom so all three `by_devid` callers in the codebase that need a
per-variant branch use one consistent shape.

The finding's alternative suggestion to "delete the redundant outer
match entirely" does **not** compile: `MembershipError` is not
`#[non_exhaustive]`, so the compiler requires a catch-all arm.
`unreachable!()` is the right shape; it matches existing house style.

## Scope

- Site A: `cli/src/recover.rs:1610-1628` inside `live_pool_matches_membership`.
- Site B: `cli/src/recover.rs:1687-1714` inside the recovery-replay loop
  that walks `expected.by_devid(devid)` to rebuild membership.

No other files change. No tests change (see Verification).

## Changes

### Site A: `cli/src/recover.rs` -- `live_pool_matches_membership`

Replace the existing `match membership.by_devid(devid) { ... }` block
inside the for-loop (currently `recover.rs:1610-1628`) with:

```rust
match membership.by_devid(devid) {
    Ok(Some((uuid, _))) => {
        fallback_uuids.insert(uuid.clone());
    }
    Ok(None) => {
        return Err(JournaledSnapshotError::NoMemberForDevid { devid });
    }
    Err(membership::MembershipError::DuplicateDevid { devid, members }) => {
        return Err(JournaledSnapshotError::DuplicateDevid { devid, members });
    }
    Err(
        other @ (membership::MembershipError::Corrupt { .. }
        | membership::MembershipError::Conflict(_)
        | membership::MembershipError::Io { .. }
        | membership::MembershipError::Save { .. }),
    ) => {
        unreachable!(
            "by_devid cannot return this MembershipError variant: {other:?}"
        );
    }
}
```

The change is structural only: the previous `Err(e) => return Err(...
match e { ... })` no-op transform is removed; the unreachable arm now
documents the invariant instead of laundering it through
`NoMemberForDevid`.

### Site B: `cli/src/recover.rs` -- recovery-replay membership rebuild

Replace the existing `match expected.by_devid(devid) { ... }` block
(currently `recover.rs:1687-1714`) by changing only the trailing fallback
arm. Keep the three existing arms (`Ok(Some(...))`, `Ok(None)`,
`Err(MembershipError::DuplicateDevid { ... })`) verbatim, and replace
`Err(err) => return Err(RecoverError::Membership(err))` with:

```rust
Err(
    other @ (membership::MembershipError::Corrupt { .. }
    | membership::MembershipError::Conflict(_)
    | membership::MembershipError::Io { .. }
    | membership::MembershipError::Save { .. }),
) => {
    unreachable!(
        "by_devid cannot return this MembershipError variant: {other:?}"
    );
}
```

This drops the silent `RecoverError::Membership(err)` forwarding, since
it could only fire if `by_devid`'s implementation changed to construct
a non-`DuplicateDevid` error -- in which case the panic makes the
contract violation loud and traceable, instead of producing a generic
"membership error" at runtime.

## Critical files

- `cli/src/recover.rs` -- the only file edited. Both sites are inside
  `for devid in pool.missing_devids.iter().copied().chain(...)` loops.
- `cli/src/membership.rs:284-302` -- `by_devid` return-type contract;
  read-only reference.
- `cli/src/lock.rs:855-869` -- existing house style being adopted;
  read-only reference.

## Verification

1. `cargo fmt` -- formatting only; the wide `|`-pattern may rewrap.
2. `cargo clippy --workspace -- -D warnings` -- catches any leftover
   binding name collisions (note: the existing `DuplicateDevid` arm
   already shadows the outer `devid` loop variable; the new arms use
   `other @ (...)` so the loop's `devid` stays visible inside the
   panic format string).
3. `just test-rust` -- runs unit tests including
   `live_pool_matches_membership_rejects_null_underlying_without_expected_devid`
   (`cli/src/recover.rs:10945`) and
   `live_pool_matches_membership_propagates_duplicate_devid_from_null_underlying`
   (`cli/src/recover.rs:11081`). Both pin the surviving variants
   (`NoMemberForDevid`, `DuplicateDevid`) but do not pin error wording,
   so they remain green.
4. No VM tests are required: the cleanup is type-system structural only
   and cannot reach the `unreachable!` arms at runtime
   (`by_devid` only constructs `DuplicateDevid`).

## Out of scope

- `cli/src/lock.rs:855-869` -- already in the target shape; no churn.
- `cli/src/status.rs`, `cli/src/remove_missing.rs` -- use `?` to bubble
  up; no per-variant branching, nothing to clean up.
- Renaming or refactoring `MembershipError` or its variants. Adding
  `#[non_exhaustive]` would not help (the issue is in callers, not the
  enum).
- Splitting `by_devid`'s return type into a `ByDevidError`-style
  narrower error -- a real improvement, but a bigger plan than the
  finding warrants.
