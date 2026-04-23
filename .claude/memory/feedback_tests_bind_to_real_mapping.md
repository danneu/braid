---
name: Error-classification tests must bind to the real mapping helper, not a hand-built variant
description: When testing which error variant a callsite returns, extract the .map_err closure into a named helper and have production and tests call that helper — otherwise a regression that reverts classification passes the test
type: feedback
originSessionId: 00109177-91cb-4d3d-b6cd-fa76d9b3ab16
---
When a slice is "error semantics only" (reclassify callsite X's `.map_err`
from `Err::Validation` to `Err::PersistFailure`), the test MUST bind to the
real mapping expression, not a hand-built variant.

Wrong (what I proposed):
```rust
let underlying = save_membership_to(&m, &bad_path).unwrap_err();
let classified = RemoveError::MembershipPersistFailure(format!("...: {underlying}"));
assert!(matches!(classified, RemoveError::MembershipPersistFailure(_)));
```
-- this passes even if the real callsite at `remove.rs:181` reverts to
`RemoveError::Validation`, because the test hand-builds the variant.

Right: extract the `.map_err` closure into a named helper that both
production and tests call.
```rust
fn map_membership_persist_failure(e: MembershipError) -> RemoveError {
    RemoveError::MembershipPersistFailure(format!("failed to persist pool membership: {e}"))
}
// callsite: .map_err(map_membership_persist_failure)
// test: let classified = map_membership_persist_failure(real_underlying_error);
```
A regression that edits the helper back to `Validation` fails the test.
(A regression that bypasses the helper entirely at the callsite still
escapes, but that is caught by grep-in-CI or by a separate invariant test;
helper extraction closes the common regression path.)

**Why:** Feedback from Dan on the Section-4 slice of
`plans/wip/plan-a-refactor-that-purrfect-torvalds.md`. He rejected a plan
whose tests verified hand-built variants and message text rather than the
real mapping from underlying error -> RemoveError.

**How to apply:** Any plan whose stated purpose is "change which error
variant this callsite returns" must (1) extract the mapping as a named
helper or function reference, (2) have the production callsite use the
helper, and (3) test the helper with a real forced-failure value from the
underlying layer. Don't write tests that construct `Err::Foo(...)` by hand
and then assert it matches `Err::Foo(_)` -- that is circular.
