# Plan: Fix misleading `DiscoverError::ReadDir` wrap on post-dedup membership insert

## Context

`cli/src/discover.rs:550-560` wraps an unreachable `MembershipError` from
the defensive backstop `membership.insert(...)` call inside
`DiscoverError::ReadDir`. Because `ReadDir`'s `Display` is
`"failed to read /dev/disk/by-id: {0}"`, a future regression that ever
trips this branch would surface to operators as:

```
failed to read /dev/disk/by-id: membership insert failed after discover pre-checks: <MembershipError text>
```

-- pointing the operator at a directory read that did not fail. The
branch is dead today (the four insert axes are pre-satisfied by the
`seen_uuids` pre-pass plus the `DiskName`-keyed `members` map plus
unique by-id directory entries plus `DiskMember::new` starting with
`devid: None`), but the error variant lies about its provenance and the
defensive closure obscures the real (membership) layer. The fix is
purely structural: route the unreachable backstop through an honest
error variant so the wrap stops misleading anyone who reads the code
or hits the branch.

## Approach

1. Add a dedicated variant `MembershipInsert(#[source] crate::membership::MembershipError)`
   to `DiscoverError` in `cli/src/discover.rs`, with Display
   `"membership insert failed after discover pre-checks: {0}"`. The
   "after discover pre-checks" framing preserves the original closure's
   diagnostic value -- it tells operators this is a logic bug rather
   than a normal collision.

   Critically: do **not** annotate the inner `MembershipError` with
   `#[from]`. The variant name `MembershipInsert` is provenance-specific
   ("this came from the post-dedup insert backstop"); a blanket
   `From<MembershipError>` impl would silently funnel any future
   `MembershipError` returned via `?` in a `Result<_, DiscoverError>`
   function into this variant, recreating exactly the mislabel problem
   this plan is trying to fix. `#[source]` preserves the error chain
   without manufacturing the conversion.

2. Add a `///` doc comment above the new variant per the repo's Rust CLI
   doc-comment rule (see `AGENTS.md` -- new `pub` variants not covered
   by an enum-level doc must justify their boundary). The text should
   explain that this is the defensive post-discover-precheck insert
   backstop -- i.e. an unreachable-in-practice logic-bug surface -- and
   is **not** the normal duplicate-disk path (which surfaces as
   `DuplicateUuid` or `LabelCollision`). The doc comment matches the
   shape used on the existing `DuplicateUuid` variant at
   `cli/src/discover.rs:29-34`.

3. Replace the closure body at `cli/src/discover.rs:550-560` with an
   explicit `.map_err(DiscoverError::MembershipInsert)?`:

   ```rust
   for (name, cand) in members {
       let member = DiskMember::new(name, cand.by_id);
       membership
           .insert(cand.luks_uuid, member)
           .map_err(DiscoverError::MembershipInsert)?;
   }
   ```

   Routing only this one call through the variant keeps the variant's
   provenance honest.

4. Move the existing comment that documents why the branch is
   unreachable (four axes pre-satisfied) to sit immediately above the
   `for` loop, so the invariant remains documented even though the
   defensive closure is gone. Keep the substance of the comment as-is.

## Files Modified

- `cli/src/discover.rs` -- two small edits:
  - Add the `MembershipInsert` variant (with `///` doc comment and
    `#[source]`-only inner error) alongside the existing
    `Cmd`/`ReadDir`/`LabelCollision`/`DuplicateUuid` variants
    (`cli/src/discover.rs:15-45`).
  - Replace the closure body at `cli/src/discover.rs:550-560` with
    `.map_err(DiscoverError::MembershipInsert)?` and relocate the
    unreachable-branch comment to introduce the loop.

No other files need touching:
- No exhaustive match arms on `DiscoverError` exist in the codebase
  (confirmed by Explore -- only `LabelCollision` test matches and a
  `.to_string()` rendering path in `main.rs`), so the new variant
  introduces no caller breakage.
- No tests pin the misleading "failed to read /dev/disk/by-id:
  membership insert failed ..." wording (the literal substring only
  appears in code at `cli/src/discover.rs:558` and in the historical
  plan at `plans/impl/2026-05-21-trim-discover-unreachable-comment.md`).
- No `docs/` markdown references the variant or the wording.

## Reused patterns

Six sibling error enums wrap `MembershipError`:

- `cli/src/add.rs:100` -- `AddError::Membership(#[from] MembershipError)`
- `cli/src/replace.rs:149` -- `ReplaceError::Membership(...)`
- `cli/src/unlock.rs:17` -- `UnlockError::Membership(...)`
- `cli/src/remove_missing.rs:39` -- `RemoveMissingError::Membership(...)`
- `cli/src/recover.rs:43` -- `RecoverError::Membership(...)`
- `cli/src/status.rs:351` -- `StatusError::Membership(...)`

This plan intentionally diverges from the siblings on two axes:

1. **Variant name.** Discover only feeds the post-dedup insert backstop
   into this variant -- never load/save and never the normal collision
   paths (those go through `DuplicateUuid` / `LabelCollision`). The
   narrower name `MembershipInsert` advertises that provenance.

2. **Conversion shape.** The siblings use `#[from]` because their
   variant means "any membership error from anywhere in this command";
   the blanket conversion matches the variant's semantics. Here the
   variant is provenance-specific, so the conversion must be too --
   hence `#[source]` plus an explicit
   `.map_err(DiscoverError::MembershipInsert)` at the single callsite.

The error-chaining shape (`#[source]` on the inner `MembershipError`)
still gives downstream consumers the structured cause via
`std::error::Error::source()`, matching the chaining the siblings get
through `#[from]`.

## Verification

1. `just test-rust` -- runs the full Rust unit-test suite, including
   the discover module's `tests` block (covers `LabelCollision`,
   `DuplicateUuid`, bare-discover gates, etc.). All existing tests
   should pass unchanged since none of them exercise the dead branch
   or assert on the misleading wording.

2. `cargo build -p braid-cli` -- confirms the new variant compiles and
   the explicit `.map_err(DiscoverError::MembershipInsert)` typechecks
   (no `From` impl is generated, by design).

3. Manual inspection: after the edit, search for any remaining use of
   `DiscoverError::ReadDir(std::io::Error::other(...))`. There should
   be zero matches -- the only `ReadDir` constructions left should be
   the two legitimate `std::fs::read_dir` failure sites at
   `cli/src/discover.rs:313` and `cli/src/discover.rs:317`.

No new tests are needed:
- The dead branch is still dead after the fix (the four pre-check
  axes are unchanged), so a "this branch returns
  `MembershipInsert`" test would require fabricating an
  impossible-to-construct membership state.
- Existing behavioral tests cover the reachable error variants
  (`LabelCollision`, `DuplicateUuid`, `Cmd`, real `ReadDir`).
