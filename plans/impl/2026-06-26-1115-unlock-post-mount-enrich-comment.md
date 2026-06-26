# Plan: fix the post-mount enrichment comment in `unlock.rs`

## Context

A review flagged the preamble comment above the post-mount enrichment
`match` in `cli/src/unlock.rs` (the `match probe::probe_pool(...)` block).
Investigation confirmed two real, concrete defects in the **comment** (the
code itself is correct and infallible-by-construction):

1. **Structure mismatch.** The comment enumerates *three* "tolerated
   outcomes," the first of which -- `Ok(PoolState { mounted: false,
   devices: vec![] })` -- reads as a distinct branch. The `match` has only
   *two* arms (`Ok`, `Err`); the code never branches on `mounted`. The
   `mounted: false` race is just an empty-`devices` sub-case of the `Ok`
   arm, where `enrich_from_pool_state` (infallible, UUID-matched-only --
   `cli/src/membership.rs#enrich_from_pool_state`) no-ops. The terser
   sibling comments for the *same* construct in `cli/src/add.rs` (bootstrap)
   and `cli/src/replace.rs` do not enumerate this sub-case at all,
   confirming it is over-documented here.

2. **Stale/incomplete test citation.** The `Pinned by` line cites only 2 of
   the **4** tests that actually guard this block. It omits
   `unlock_tolerates_post_mount_probe_err` and -- more importantly --
   `unlock_tolerates_post_mount_save_membership_failure`, which is the
   *only* test pinning the comment's third ("save failure") outcome.

Intended outcome: a comment whose structure mirrors the code's two arms,
matches the sibling register, preserves the one genuinely unlock-specific
fact (unlock *warns* on save-failure where add/replace `?`-fail), and
cites all four guarding tests. This is the "ideal, simple, most correct"
fix per `AGENTS.md`, and it dissolves the class of review finding that
keeps landing on this block.

## Change (single file, comment-only)

**File:** `cli/src/unlock.rs#UnlockPlan::execute` -- the preamble comment
immediately above `match probe::probe_pool(runner, fs, mount_point)`. (The
block lives in `UnlockPlan::execute`, not `cmd_unlock`.)

Replace the current three-bullet comment with a two-arm version:

```rust
        // Enrich pool.json with live metadata (devid, added_at) from a
        // fresh pool probe, best-effort: correctness never depends on it
        // (see the contract above). The in-memory membership clone is
        // authoritative here because the Rust dispatch holds the pool
        // flock for unlock's lifetime. Neither arm converts the
        // already-completed mount into a failure:
        //   * Ok  -- enrich the UUID-matched members and save. A raced
        //            probe (mounted: false, no pool devices) makes
        //            enrich_from_pool_state a no-op; a save failure only
        //            warns (add/replace `?`-fail here, but unlock's pool
        //            is already online).
        //   * Err -- warn and leave pool.json unrewritten (e.g. a parser
        //            drift in `btrfs filesystem show`).
        // Pinned by unlock_tolerates_post_mount_probe_mounted_false,
        // unlock_tolerates_post_mount_save_membership_failure,
        // unlock_warns_when_post_mount_probe_errors, and
        // unlock_tolerates_post_mount_probe_err.
```

Notes for the implementer:
- ASCII only (`--`, backticks, no curly quotes), per the user's writing
  style and project convention. Inline `//` comments are exempt from
  `check-output-ascii.py`, but keep ASCII anyway.
- The four pinned test names exist verbatim at `cli/src/unlock.rs`:
  `unlock_warns_when_post_mount_probe_errors`,
  `unlock_tolerates_post_mount_probe_mounted_false`,
  `unlock_tolerates_post_mount_probe_err`,
  `unlock_tolerates_post_mount_save_membership_failure`. The `Pinned by`
  list above groups them by arm (two Ok-arm pins, then two Err-arm pins).
- Do not touch the `match` body or any executable code -- behavior is
  unchanged.

## Explicitly out of scope (and why)

- **No test changes.** All four tests assert distinct, non-redundant
  properties; none is dead. In particular the `mounted_false` test's
  preamble is *accurate* -- its "distinct from real probe errors" wording
  refers to the no-warning **outcome** vs. the `Err` arm, not to a third
  match arm -- so it needs no edit. (This rebuts the part of the original
  finding that suggested the test comment claimed a distinct code path.)
- **No sibling-file changes.** `add.rs` (bootstrap) and `replace.rs`
  document the same construct correctly and tersely; they `?`-fail on save
  (no save-failure-tolerated outcome to document) and never enumerated the
  `mounted: false` sub-case, so there is nothing to fix there.
- **No `Pinned by` enforcement tooling.** braid has a checker for doc
  `## See` paths but not for inline test pins; adding one is scope creep
  for a Low-severity comment fix.

## Verification

Comment-only, so the goal is "no behavior change + the new prose is
accurate":

1. `just test-rust` -- the full Rust suite still builds and passes
   (proves no code was accidentally altered).
2. Targeted: run the guarding tests and confirm they pass --
   `cargo test -p braid-cli post_mount` (crate is `braid-cli`; the
   `post_mount` substring filter runs all four unlock pins, plus the
   add/replace post-mount siblings, which is harmless).
3. `grep -n "unlock_tolerates_post_mount\|unlock_warns_when_post_mount" cli/src/unlock.rs`
   -- confirm every test named in the new `Pinned by` list still exists.
4. Human read: each bullet maps to exactly one `match` arm, and the
   `Ok`-arm bullet covers both the empty-devices no-op and the
   warns-on-save-failure sub-cases.
