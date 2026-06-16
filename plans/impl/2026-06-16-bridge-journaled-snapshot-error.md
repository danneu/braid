# Refactor: bridge `JournaledSnapshotError` into `RecoverError` via `From`

## Context

`cli/src/recover.rs` defines a small private enum `JournaledSnapshotError`
(`recover.rs#JournaledSnapshotError`) with two corruption variants. The
predicate `live_pool_matches_membership` returns
`Result<bool, JournaledSnapshotError>`, and **six** call sites each re-map its
two error variants into `RecoverError::DuplicateDevidDuringReplay` /
`RecoverError::NoMemberForJournaledDevid` with byte-identical match arms (12
arms, ~36 lines). The mapping is 1:1 and lossless (same `devid` / `members`
payloads, same field types), so the inline arms are pure boilerplate that must
be edited in lockstep and force a reader to cross-reference two parallel error
types.

This is reinforced by a sibling: `recover_membership_matching_expected`
(`recover.rs#recover_membership_matching_expected`) runs the *same* `by_devid`
walk and maps the *same* two outcomes straight to those `RecoverError` variants
with no intermediate type — proving the stopover is gratuitous for the error
path. An earlier plan even recorded that the arms were written inline only
*because* the variants "cannot be auto-bubbled through `?`" — which is exactly
what a `From` impl fixes.

**Goal:** centralize the 1:1 mapping in one `impl From<JournaledSnapshotError>
for RecoverError`, let `?` carry the conversion, and collapse each call site to
an `if`/expression that keeps its own `Ok(false)` topology-mismatch wording.
Behavior is unchanged.

**Keep the enum.** It is a deliberate narrow contract: it keeps corruption
signals type-distinct from the `Ok(false)` topology-mismatch case. That intent
is pinned by `plans/impl/2026-05-18-anchor-recovererror-corruption-bridge-tests.md`
and by two unit tests that match on `JournaledSnapshotError` directly. Collapsing
the predicate to return `RecoverError` would widen its error contract, churn
those unit tests, and dissolve the distinction the project deliberately
established. The `From` impl removes the duplication while preserving the type.

## Precedent and conventions (reuse, don't invent)

- **Template:** `lock.rs#From<CloseMapperError>` (`lock.rs:42-50`) and
  `probe.rs#From<OwnershipError>` are manual, match-based `From` impls that
  translate semantically-distinct variants 1:1 — exactly this shape. Mirror
  `lock.rs` verbatim in structure.
- **Doc comments:** `docs/dev/doc-comments.md` puts `From` impls on the skip
  list (the trait documents intent); the `lock.rs` precedent is comment-free.
  So the new `From` impl gets **no** `///`. The "why the type still exists"
  rationale stays on the `JournaledSnapshotError` enum doc comment (updated
  below), which is the right home and pre-empts the next "collapse it" finding.
- **ASCII:** no new user-facing strings are introduced; the per-site `Ok(false)`
  wording is preserved verbatim. `just check-output-ascii` stays green.

## Changes — all in `cli/src/recover.rs`

### 1. Add the `From` impl (immediately after the enum, ~line 91)

Mirror `lock.rs#From<CloseMapperError>`; no doc comment per convention:

```rust
impl From<JournaledSnapshotError> for RecoverError {
    fn from(value: JournaledSnapshotError) -> Self {
        match value {
            JournaledSnapshotError::DuplicateDevid { devid, members } => {
                RecoverError::DuplicateDevidDuringReplay { devid, members }
            }
            JournaledSnapshotError::NoMemberForDevid { devid } => {
                RecoverError::NoMemberForJournaledDevid { devid }
            }
        }
    }
}
```

### 2. Update the `JournaledSnapshotError` enum doc comment (`recover.rs:77-81`)

The current text says the variants are "Bridged into the matching
`RecoverError::*` variant **at each call site**" — stale after this change, and
it does not explain why the type survives a 1:1 mapping. Replace with wording
that (a) drops "at each call site", (b) points at the `From` impl, and (c)
states the surviving reason: corruption stays type-distinct from the `Ok(false)`
topology-mismatch case that each call site words itself. Roughly:

```rust
/// Recover-local snapshot-walk errors raised by `live_pool_matches_membership`
/// when `journal.pre_membership` / `journal.target_membership` corruption
/// prevents the gate from evaluating its predicate. A dedicated type -- rather
/// than returning `RecoverError` directly -- keeps these corruption signals
/// type-distinct from the `Ok(false)` topology-mismatch case, which each call
/// site reports with its own `RecoverError::Failed` wording. The
/// `From<JournaledSnapshotError>` impl below bridges the two corruption variants
/// into `RecoverError::{DuplicateDevidDuringReplay, NoMemberForJournaledDevid}`
/// so `?` carries them across each call site.
```

### 3. Collapse the six call sites

Each enclosing function returns `Result<_, RecoverError>`, so `?` auto-converts
through the new `From` impl. Two shapes:

**Shape A -- five sites that discard the bool** (`recover.rs:2664`, `2688`,
`2760`, `3065`, `3181`): the `Ok(true) => {}` / `Ok(false) => Err(Failed(..))` /
two `Err` arms collapse to a guard that preserves the exact per-site message
(verbatim -- three use `format!("... {devid} ...")`, two use a plain `.into()`):

```rust
if !live_pool_matches_membership(&pool, &journal.pre_membership)? {
    return Err(RecoverError::Failed(format!(
        "remove-missing recovery found devid {devid} still missing, but live pool \
         topology does not match the pre-operation membership"
    )));
}
```

**Shape B -- one site that uses the bool** (`recover.rs:3054`, in
`execute_replace_pool_mutation_recovery`): becomes a single expression:

```rust
let pre_topology =
    live_pool_matches_membership(&pool, &journal.pre_membership)? && !live.contains(new_uuid);
```

Net: 12 inline `Err` arms + 6 `match` scaffolds removed; one ~12-line `From`
impl added; each site materially shorter.

## What does NOT change

- **`live_pool_matches_membership` signature** stays `Result<bool,
  JournaledSnapshotError>`. Its internal construction sites (`recover.rs:1631`,
  `1634`) are untouched.
- **Tests: zero changes.**
  - Boundary tests `bridges_duplicate_devid_corruption_to_typed_recover_error`
    and `bridges_no_member_for_devid_to_typed_recover_error`
    (`recover.rs:6617`, `6680`) assert the *final* `RecoverError` variant -- the
    `From` impl yields the identical result, and these tests are precisely what
    compile-anchors the contract through the refactor.
  - Direct unit tests `live_pool_matches_membership_rejects_null_underlying_without_expected_devid`
    and `live_pool_matches_membership_propagates_duplicate_devid_from_null_underlying`
    (`recover.rs:11867`, `12063`) match on `JournaledSnapshotError`, which still
    exists with the same variants -- they keep compiling and passing.
- **Sibling `recover_membership_matching_expected`** is left as-is. Considered
  unifying its `by_devid` error arms with this walk, but the two helpers differ
  in purpose (predicate-returning-bool vs membership-materializer) and only
  share the small mapping skeleton; forcing a shared helper would over-couple
  them for little gain. Rejected.
- **Historical plan docs** under `plans/impl/` are dated records and are not
  edited; the living rationale moves onto the enum doc comment (change 2).

## Verification

1. `just test-rust` -- full CLI suite compiles and passes (the four named tests
   above in particular; run e.g. `cargo test --lib bridges_` and
   `cargo test --lib live_pool_matches_membership` to spot-check).
2. `just clippy` (`cargo clippy --manifest-path cli/Cargo.toml --tests`) -- clean;
   confirms no `needless_return` / unused-import fallout from the rewrite.
3. `just check-output-ascii` -- green (no new strings; messages preserved verbatim).
4. Sanity diff: confirm the six call sites now read as `if !...?` / expression,
   the `From` impl is the sole mapping site, and grepping `JournaledSnapshotError`
   shows only: the enum def, the two construction sites in
   `live_pool_matches_membership`, the new `From` impl, and the two unit-test
   assertions -- no remaining inline bridge arms.
