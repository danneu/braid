# Plan: trim the unreachable-branch comment in `discover_from_dir_inner`

## Context

A code review flagged the 12-line comment block at `cli/src/discover.rs:519-530` as a draft-quality stream of consciousness: it enumerates and rejects three error-mapping alternatives in prose, then lands on `DiscoverError::ReadDir(io::Error::other(...))` while noting the branch is unreachable. The verify-issue investigation confirmed:

- The block was introduced in `844ed0f wip: migrate discover + lock + parser (phase 4a)` and never revisited -- in-progress reasoning that survived to mainline.
- The chosen wrap is correct as a defensive backstop; the cited code is not buggy.
- The finding's proposed alternative -- a new `DiscoverError::Internal(String)` variant -- would invent a brand-new pattern (no `Internal` variant exists anywhere under `cli/src/`) to soften the prefix of an unreachable error. Gold-plating.
- The finding's proposed replacement text uses `L491-509`/`L453-479` line-number breadcrumbs, which would directly violate the policy set today (2026-05-21) in commit `1fe9651 docs: drop rust line-number refs from comments` and its plan at `plans/impl/2026-05-21-drop-rust-line-number-refs.md` ("Those numerals drift as the Rust files grow, so keep durable symbol references instead").

The right work is the smallest one: shrink the comment to capture intent + invariant, keep code unchanged.

## Scope

**One file, one block.** `cli/src/discover.rs:519-530` (the `//`-prefix lines inside `membership.insert(...).map_err(|e| { ... })`).

**No code changes.** Keep:
- The `DiscoverError::ReadDir(std::io::Error::other(format!(...)))` wrap at L531-533.
- The `"membership insert failed after discover pre-checks: {e}"` message body (it appears verbatim in the wrapped `io::Error` and survives the `ReadDir` Display prefix, so a future regression remains diagnosable).
- All surrounding logic (the pre-passes, the for-loop, the `?` propagation).

**No new variant.** Do not add `DiscoverError::Internal`. The misleading-prefix concern is purely theoretical because the branch is provably unreachable; introducing a one-off variant to soften an error message that nobody will see is unjustified and would be the only `Internal`-style variant in the crate.

## Replacement intent

A short comment (target 4-6 lines) that conveys:

1. **Why unreachable -- per axis.** `PoolMembership::insert` (at `cli/src/membership.rs`) checks four uniqueness axes. The comment must name each one and the *specific* reason it is pre-satisfied here, because the four reasons are different:
   - **Axis 1 (UUID).** Covered by the `seen_uuids` duplicate-UUID pre-pass earlier in this same function.
   - **Axis 2 (disk-name).** Covered by `members` being keyed on `DiskName` (the alias-dedup `match` on `members.entry(disk_name)` collapses duplicates by construction).
   - **Axis 3 (by-id).** Safe because by-id values come from a single `read_dir` of `/dev/disk/by-id/`, whose entries are unique by directory semantics. Neither named guard covers this -- it is a separate structural fact about the iteration source.
   - **Axis 4 (non-None devid).** Safe because `DiskMember::new` (at `cli/src/membership.rs:408-415`) constructs members with `devid: None`, and the axis-4 check is gated on `Some(devid)`. Neither named guard covers this either; it is a property of the constructor.
2. **Why the wrap exists anyway.** Defense-in-depth: if any of the four invariants ever regresses, the `MembershipError` text is preserved verbatim inside the wrapped `io::Error` so the offending detail still reaches the operator (the `DiscoverError::ReadDir` Display prefix becomes misleading in that scenario, but the diagnostic body survives).

Candidate wording (the implementer may tighten further, but must keep the per-axis breakdown so the comment does not overstate what the two named guards cover):

```rust
// Unreachable: insert's four axes are all pre-satisfied here. Axis 1
// (UUID) by the seen_uuids pre-pass above; axis 2 (name) by members
// being keyed on DiskName; axis 3 (by-id) by read_dir yielding unique
// directory entries; axis 4 (devid) by DiskMember::new starting with
// devid: None. Wrap defensively so any future regression surfaces the
// MembershipError text verbatim.
```

This matches the prevailing comment style in the same file (e.g. the `// Per-disk-name best candidate, keyed by the validated DiskName.` block at the top of `discover_from_dir_inner`): symbol references, intent + invariant in a few lines, no enumerated rejected alternatives. It is a few lines longer than a minimal version, but the extra lines pay for themselves by making each invariant individually checkable -- a maintainer who weakens one of the four guards will see exactly which line of the comment becomes false.

## Out of scope

- Adding `DiscoverError::Internal`. Defer until a reachable internal-bug path appears.
- Changing the `ReadDir` Display prefix or the wrap shape.
- Touching any other `io::Error::other` site in `cli/src/`. The codebase uses this idiom widely (~15 sites in `membership.rs`, `journal.rs`, `alert.rs`, etc.); they're at genuine API boundaries and are fine.
- Doc-comment policy. `AGENTS.md`'s "Doc Comments" section governs `///` comments on `pub`/`pub(crate)` items; this is an internal `//` line comment inside a function and is unaffected.

## Verification

This is a comment-only change with no behavior delta. Sufficient checks:

1. `just test-rust` -- ensures the file still compiles cleanly and existing unit tests pass.
2. `cargo clippy --workspace --all-targets -- -D warnings` (or whatever the project's lint gate is invoked as via `just`) -- catches stray warnings introduced by the edit.
3. Skip VM tests (`just test-vm`, `just test-parsers`): they exercise runtime behavior that is unchanged.

No new test is needed. The four invariants the comment encodes are a mix of behavioral and structural facts -- only two of the four (UUID and disk-name uniqueness) are user-visible regressions, and those are already covered by the existing duplicate-UUID and label-collision tests in this file's `#[cfg(test)]` block plus the `insert_rejects_duplicate_*` tests at `cli/src/membership.rs:875-955`. The other two (by-id uniqueness via `read_dir`, devid-None via `DiskMember::new`) are structural code facts that would only be falsified by an intentional refactor; a maintainer doing that refactor will see the relevant line of the comment become false and update accordingly.

## Files touched

- `cli/src/discover.rs` -- replace the 12-line comment block in the `map_err` closure of `discover_from_dir_inner`'s final `for (name, cand) in members` loop with a 2-4 line note per the wording above.

No other files.
