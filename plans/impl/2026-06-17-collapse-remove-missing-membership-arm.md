# Plan: collapse the unreachable `RemoveMissingError::Membership` arm

## Context

`RemoveMissingError::Membership(#[from] membership::MembershipError)`
(`cli/src/remove_missing.rs`) is a fail-closed error variant that is
**unreachable on the production path** and carries a 13-line justification
comment. `plan_remove_missing` loads membership through `load_membership`
-- whose devid-uniqueness sweep (`membership::load_membership_from`) rejects a
duplicate-devid `pool.json` as `Validation("failed to load pool membership:
...")` -- *before* `resolve_removal_target` ever calls `by_devid`. The only `?`
that feeds the `Membership` arm is `membership.by_devid(devid)?`, and `by_devid`
can return only `MembershipError::DuplicateDevid` (`membership.rs#by_devid`:
0 -> `Ok(None)`, 1 -> `Ok(Some)`, _ -> `DuplicateDevid`). So the arm cannot fire.

This arm is a **finding magnet**: it has now drawn two review findings of
opposite polarity. Commit `fcedb3b6` added the 13-line comment specifically to
answer a prior reviewer who misread the arm as a *live, untested* operator path
and asked for a test of a case that cannot occur. That comment is now drawing the
opposite finding -- "dead reasoning overhead, remove it." Adding prose did not
make the arm stop attracting findings, because a standalone typed-but-dead enum
variant *reads* as a live error path until a reader finishes a paragraph proving
it isn't.

**Intended outcome:** delete the variant and handle `by_devid`'s error inline in
`resolve_removal_target`, folding it into the existing `Validation(String)`
variant with a short comment at the resolution point. This removes the artifact
reviewers keep tripping on, removes an over-broad `#[from]` (it imports the whole
five-variant `MembershipError` enum to carry one unreachable variant from one
call site), and **preserves fail-closed behavior verbatim**. Two complementary
tests cover it: the existing load-gate test pins the production path (the load
gate refuses duplicate devids before resolution is reached), and a new direct
unit test on `resolve_removal_target` pins the retained fail-closed arm itself --
which the load-gate test cannot reach.

This is not merely a simplicity tidy: it conforms the code to a documented
mutation-safety heuristic -- `docs/dev/safety-heuristics.md`: "Keep diagnostic
refinements out of mutating-command state enums when the new distinction only
matters for `status`, `doctor`, TUI, or error rendering." The `Membership`
variant's *only* distinction from `Validation` is its rendered `#[error("pool
membership corruption: {0}")]` prefix -- a rendering-only distinction. The same
doc's fail-closed rule ("If a branch can corrupt state ... every uncertainty in
that branch is a hard error") is satisfied by the collapsed form: it is still a
hard error that refuses before any mutation. No principle or ADR changes.

## Why collapse rather than trim the comment

The finding offers two options. Collapsing is strictly better than trimming the
comment to one line:

- Trimming leaves the *variant* and the over-broad `#[from]` in place, so a
  reader still has to confirm `Membership` is dead -- the exact reasoning
  overhead the finding objects to.
- Collapsing is observably behavior-preserving. No code matches on
  `RemoveMissingError::Membership` anywhere (the sole consumer, `main.rs`,
  renders any error via `e.to_string()` and `exit(1)` with no per-variant
  branch), and no test asserts the variant or its message. Its entire observable
  contribution is the prefix string, which is preserved by
  `Validation(format!("pool membership corruption: {e}"))`.

## The change (one file: `cli/src/remove_missing.rs`)

### 1. Delete the `Membership` variant + its 13-line comment

Remove the doc block and the two variant lines (currently the
`/// Defense-in-depth refusal ...` comment, `#[error("pool membership
corruption: {0}")]`, and `Membership(#[from] membership::MembershipError)`).
The remaining `RemoveMissingError` variants (`Validation`, `NoMemberForDevid`,
`Probe`, `Pool`) are unaffected; `Probe`/`Pool` keep their own `#[from]`s.

### 2. Rewrite `resolve_removal_target` to handle `by_devid`'s error inline

Replace the `?`-propagating body and rewrite the doc comment (which currently
ends with the now-dangling `(see \`RemoveMissingError::Membership\`)`):

```rust
/// Resolve a missing devid to a `(LuksUuid, DiskName)` pair via
/// `PoolMembership::by_devid`. Returns `RemoveMissingError::NoMemberForDevid`
/// when no member carries the persisted devid (so the operator can decide
/// whether enrichment ever ran on the pool). This is the single point of
/// identity resolution for `remove-missing` -- callers thread the returned
/// UUID straight into the journal and the persisted-member removal.
fn resolve_removal_target(
    devid: Devid,
    membership: &membership::PoolMembership,
) -> Result<(LuksUuid, DiskName), RemoveMissingError> {
    match membership.by_devid(devid) {
        Ok(Some((uuid, member))) => Ok((uuid.clone(), member.name.clone())),
        Ok(None) => Err(RemoveMissingError::NoMemberForDevid { devid }),
        // Unreachable on the production path: the sole caller
        // (`plan_remove_missing`) resolves against a `load_membership`-validated
        // snapshot whose devid-uniqueness sweep already refuses duplicate devids
        // (pinned by `cmd_remove_missing_duplicate_devid_pool_json_refused_at_load`).
        // Kept fail-closed because remove-missing mutates: surface the corruption
        // and refuse rather than act on a device chosen from a corrupt map.
        Err(e) => Err(RemoveMissingError::Validation(format!(
            "pool membership corruption: {e}"
        ))),
    }
}
```

Notes:
- `by_devid` returns `Result<_, MembershipError>` and `MembershipError: Display`
  (`thiserror`), so `format!("...{e}")` works; no new imports.
- The fail-closed rationale moves from the variant to the resolution point,
  where the unreachability actually lives, and shrinks from 13 lines to ~5 while
  still pointing at the load-gate test.

### 3. Add a `resolve_removal_target` unit test pinning the fail-closed arm

The new `Err(e)` arm is unreachable through `cmd_remove_missing` -- the load gate
intercepts duplicate devids first -- so no command-level test exercises it,
including the existing `cmd_remove_missing_duplicate_devid_pool_json_refused_at_load`,
which pins the *load gate*, not this arm. The arm would still compile and that
test would still pass if a future edit swallowed it (mapped the error to `Ok` or
`NoMemberForDevid`). The defense-in-depth property the plan keeps the arm for is
precisely a behavioral contract of `resolve_removal_target`, so pin it directly.
This is the test the original `2026-06-09-...` plan correctly declined when the
arm was a pass-through `#[from]` variant (redundant with `membership.rs`'s
`by_devid` test, and structure-sensitive because it asserted a dead variant);
that rejection no longer holds, because this plan turns the helper into an active
mapping (`MembershipError` -> the corruption-prefixed `Validation` string) that
nothing else pins.

Add to `#[cfg(test)] mod tests` (direct unit tests of private helpers are already
the norm in this file -- `check_relocation_space`, `format_remove_missing_confirm`):

- Build a corrupt membership with `PoolMembership::for_corruption_tests(...)`
  (already used by the load-gate test) holding two members with distinct UUIDs
  that both carry `devid: Some(3)`.
- Call `resolve_removal_target(Devid::new(3), &membership)` directly.
- Assert `Err(RemoveMissingError::Validation(msg))` where `msg` contains:
  - `"pool membership corruption"` -- the prefix this arm adds. **Load-bearing
    anti-swallow assertion**: it proves the `Err(e)` arm fired rather than
    collapsing to `Ok`/`NoMemberForDevid`.
  - `"duplicate devid"` -- the inner `MembershipError::DuplicateDevid` cause
    threaded through `{e}`. Verified against its Display in `membership.rs`:
    `"duplicate devid {devid} in pool membership across UUIDs ..."`.
- Suggested name: `resolve_removal_target_duplicate_devid_refuses_fail_closed`.
- Preamble per `docs/dev/testing.md`: *Intent* -- `resolve_removal_target`
  refuses fail-closed with a corruption message when handed a duplicate-devid
  membership. *Why it exists* -- pins the retained defense-in-depth arm that the
  production load gate makes unreachable, so a future edit that swallowed it, or a
  future caller resolving against an unvalidated membership, is caught here.
  *Scenario* -- a corrupt `pool.json` reaches resolution directly (a hypothetical
  future caller that skipped the load gate); resolution must refuse, not pick one.

### 4. Update the three stale references in the test module

In `cmd_remove_missing_duplicate_devid_pool_json_refused_at_load`, three `//`
comments name the now-deleted variant; the test *body* is unchanged (it still
asserts `RemoveMissingError::Validation` from the load gate, which is correct):

- Preamble "Why it exists" (~`:3465`/`:3467`): reword "pins the premise the
  `RemoveMissingError::Membership` and `resolve_removal_target` doc comments
  rely on -- ... the `Membership` arm is unreachable" to reference the load gate
  owning the refusal without naming the deleted variant.
- Assertion comment (~`:3520`): "not the unreachable `Membership` arm and not an
  incidental load failure" -> drop the `Membership` arm clause.

## Out of scope (deliberately rejected)

- **Tightening `by_devid`'s return type** to a dedicated single-variant error
  (it can only return `DuplicateDevid`) would dissolve the over-broad enum at the
  type level, but touches all five callers (`status.rs` x2, `recover.rs` x2,
  `lock.rs` x1, plus the `membership.rs` test) -- disproportionate to a
  Low/Simplicity finding and outside its root cause. Not done.
- **Unifying with `status::build_devid_names`** (which swallows the same
  `DuplicateDevid` via `.ok().flatten()`): the divergence is intentional and
  correctly cross-documented -- read-only display must not abort; the mutating
  path must fail closed. Do not unify.
- **`unreachable!()`** (as `status.rs#membership_load_advisory` uses for the same
  variant): wrong here -- panicking in a *mutating* command contradicts the
  fail-closed ethos. `Validation` keeps it a clean refusal.
- **Editing the frozen plan docs** under `plans/impl/` that mention the variant
  (`2026-06-09-...`, `2026-05-28-...`, `2026-05-13-...`): historical records of
  past decisions, not live docs. This plan supersedes the
  `2026-06-09-...` plan's "Keep the `?` propagation and the `Membership` variant"
  decision; leave the record intact.

## Verification

1. `just test-rust` (or `cargo test -p braid-cli`) -- whole CLI suite green. Two
   distinct pins:
   - The new `resolve_removal_target_duplicate_devid_refuses_fail_closed` (step 3)
     is the pin for the retained fail-closed arm: it must **fail** if the `Err(e)`
     arm is swallowed or the corruption prefix dropped.
   - The existing `cmd_remove_missing_duplicate_devid_pool_json_refused_at_load`
     pins the *production path* and passes unchanged: the load gate is untouched,
     so the refusal is still `Validation` containing `failed to load pool
     membership` and `devid '3' already in use`.
   The happy-path test that uses `by_devid` as a lookup helper
   (`cmd_remove_missing_resolves_devid_to_uuid_...`) is unaffected.
2. `cargo doc -p braid-cli --no-deps` -- builds clean. (The line-63 backtick
   reference to the deleted variant is being rewritten away; it was never a
   `[...]` intra-doc link, so there is no link to break either way.)
3. `cargo clippy -p braid-cli` -- no new warnings; the three-arm `match` is
   idiomatic and the variant removal cannot regress the existing
   `large-err`/`too-many-args` allowances.
4. ASCII check (`scripts/docs/check-output-ascii.py`): the new runtime string
   `pool membership corruption: {e}` is ASCII (comments are exempt regardless).
5. Manual read: `resolve_removal_target` now states the unreachability at the
   resolution point and reads consistently with `status::build_devid_names`;
   `RemoveMissingError` no longer carries a variant a reader must prove dead.
