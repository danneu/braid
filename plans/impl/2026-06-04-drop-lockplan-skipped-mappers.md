# Drop dead `LockPlan.skipped_mappers` planner output

## Context

`LockPlan.skipped_mappers` (a `pub Vec<MapperName>`) and its backing
`CloseSetAccumulator.skipped_mappers` are **write-only in production**. They are
populated at two skip sites but never read by any production path -- every code
reference lives in `cli/src/lock.rs` (`git grep -n skipped_mappers -- cli/src`;
the only other tracked matches are archived `plans/impl/` design docs), and every
value-read there is inside `#[cfg(test)] mod tests`. `LockPlan::preview` and
`LockPlan::execute`
derive everything they emit from `notes` (`PreviewNote::Warn`) plus
`cleanup_uncertain`.

This is a redundant fourth representation of "this mapper was skipped." Each skip
site already does three things in lockstep:

1. push a `PreviewNote::Warn` that **names the mapper**
   (`skipped_mapper_warn_body` / `duplicate_devid_warn_body`),
2. push to `skipped_mappers`,
3. mark cleanup incomplete (`cleanup.mark_incomplete_*`).

(1) and (3) are the real contract -- `execute` emits all `Warn` notes through one
loop and `preview` renders the cleanup-incomplete line off `cleanup_uncertain`.
(2) carries no information the warn note doesn't, and as `pub` surface it invites
a future caller to treat it as authoritative even though the warn note is the
contract. Removing it eliminates the dead state and the risk of the two
representations silently diverging. This matches the repo's "No backwards
compatibility / no speculative state" posture and the recent
`decb9d61 refactor(lock): drop dead execute membership parameter` precedent.

`cleanup_uncertain` is a strict **superset** signal -- it is also set at the two
scan-failure paths (`build_close_sets_full` Pass-3 scan failure and
`build_close_sets_uuid_scanned_fallback` scan failure) where no mapper can be
enumerated and nothing is pushed to `skipped_mappers`. So `skipped_mappers` was
never even a complete record of uncertainty.

**Why removal is the ideal fix (vs. alternatives):** making the field non-`pub`
leaves the dead state; adding a production consumer or folding the skip warns
into a structured `Vec<SkippedMapper>` fights the established single-emit
`PreviewNote::Warn` contract (`execute` emits scan-failure, orphan, fallback, and
skip warns through one uniform loop) and would *increase* complexity. The warn
notes + `cleanup_uncertain` + the `CleanupConfidence` tri-state already form a
coherent single-sourced contract; the field is the redundant extra.

## Scope

One file: `cli/src/lock.rs`. No behavior change. No doc/ADR change (the
behavioral contract -- warn-on-skip + cleanup-incomplete -- is unchanged). No VM
tests in the blast radius; this is a planner-internal field with unit coverage
only.

## Production changes (`cli/src/lock.rs`)

1. **`CloseSetAccumulator`** (struct, ~L206-211): delete the
   `skipped_mappers: Vec<MapperName>,` field (L208). `#[derive(Default)]` stays
   valid.
2. **`push_uuid_classified_candidate`**:
   - **Doc comment** (~L350-352): currently "Classify one scanned candidate into
     the ordered close vectors **or the skipped set**." "the skipped set" names
     the structure being removed -- reword to describe the two live outcomes, e.g.
     "Classify one scanned candidate: push a member/orphan close entry, or (when
     the backing LUKS UUID cannot be verified) emit a skip warning and mark
     cleanup incomplete." Keep the load-bearing second sentence ("Keeping this
     shared between full and fallback planning prevents stranded mapper handling
     from drifting back to name inference.").
   - **Err arm** (~L372-378): delete `acc.skipped_mappers.push(mapper);` (L376).
     Keep the
     `acc.notes.push(PreviewNote::Warn(skipped_mapper_warn_body(&mapper, &cmd_err)))`
     and `acc.cleanup.mark_incomplete_unclassified()`. (`mapper` is only borrowed
     by the warn body after this edit, then dropped -- no move/borrow fallout.)
3. **`build_close_sets_full`** Pass-2 `DuplicateDevid` arm (~L1009-1020): delete
   `acc.skipped_mappers.push(nu.mapper.clone());` (L1018). Keep the
   `duplicate_devid_warn_body` push and `acc.cleanup.mark_incomplete_classified()`.
4. **`LockPlan`** (struct, ~L594-610): delete the field and its doc comment
   (L601-603).
5. **`plan_lock`** `LockPlan { .. }` construction (~L929-942): delete
   `skipped_mappers: acc.skipped_mappers,` (L938). This is the only construction
   site.

`MapperName` stays heavily used elsewhere, so no orphaned import.

## Test changes (`cli/src/lock.rs`, `mod tests`)

All assertions on `skipped_mappers` are removed. Most are pure deletions because
the same test already asserts the warn note and/or the cleanup signal beside
them; two are replaced with the structure-insensitive equivalent to preserve
intent; one stale preamble comment is updated.

**Pure deletions** (adjacent assertion already covers the signal):

| Site (current line) | Test | Already-present sibling coverage |
| --- | --- | --- |
| L3011 | `fallback...empty` (negative) | `assert!(!plan.cleanup_uncertain)` (L3012) |
| L3044 | `fallback_member_named_mapper_with_different_uuid_is_orphan` | `assert!(!plan.cleanup_uncertain)` (L3045) |
| L3172 | `unverified_fallback_candidate_is_warned_and_skipped` | rendered output asserts `[warn] skipping mapper braid-aaa: cannot verify...` + "cleanup incomplete" (L3173-3179) |
| L4668 | `..._orphan_..._warns` | orphan warn check (L4660) + `!acc.cleanup.is_uncertain()` (L4669) |
| L4702 | `full_arm_pass2_null_underlying_unknown_devid...` | orphan warn check (L4694) + `!acc.cleanup.is_uncertain()` (L4703) |
| L4756 | `full_arm_pass2_duplicate_devid_skips_and_warns...` | `acc.cleanup.is_uncertain()` (L4757) + warn-content checks incl. `warns.len()==1` (L4759-4773) |
| L4872-4875 | `uuid_scanned_fallback_preserves_member_then_orphan_close_order` | `!acc.cleanup.is_uncertain()` (L4876-4879) |
| L4960 | `full_arm_stranded_mapper_classify_failure_skips_candidate` | `acc.cleanup.is_uncertain()` (L4963) + "skipping mapper braid-stranded" warn (L4964-4972) |

**Replacements** (no sibling coverage; substitute the structure-insensitive form):

- **L4927-4930**, `uuid_scanned_fallback_malformed_mapper_with_uuid_is_orphan`:
  replace `assert!(acc.skipped_mappers.is_empty(), "readable UUID should not be skipped")`
  with `assert!(!acc.cleanup.is_uncertain(), "readable UUID should not be skipped")`.
  (Orphan classification neither skips nor marks cleanup uncertain, so this keeps
  the explicit "not skipped" intent.)
- **L4996-4999**, `full_arm_pass3_classify_failure_suppresses_known_closed_members`:
  this test has *no* warn assertion -- replace
  `assert_eq!(plan.skipped_mappers, vec![MapperName("braid-stranded".to_owned())])`
  with a warn-note assertion mirroring L4964-4972:
  ```rust
  assert!(
      plan.notes.iter().any(|note| matches!(
          note,
          PreviewNote::Warn(body) if body.contains("skipping mapper braid-stranded")
      )),
      "skip warning for braid-stranded expected, got: {:?}",
      plan.notes
  );
  ```
  (`plan.notes` is `pub`; the wording is exact -- `skipped_mapper_warn_body`
  emits `"skipping mapper {entry}: cannot verify backing LUKS UUID (...)"`.)

**Preamble comment** (L4706-4709, `full_arm_pass2_duplicate_devid...`): the
Intent currently says the entry "lands exactly once in skipped_mappers." Reword
to reference the single `DuplicateDevid` warn -- the "exactly once / Pass 3 must
not rescan" guarantee is pinned by `assert_eq!(warns.len(), 1, ...)` at L4767,
not by the removed field. Suggested: "surfaces a typed `DuplicateDevid` warn
exactly once and sets `cleanup_uncertain`; Pass 3 must not rescan the skipped
mapper."

## Why coverage is preserved

- "A mapper was skipped" is still proven by the named warn note at every skip
  site, and "cleanup is incomplete" by `cleanup_uncertain` -- both
  behavioral/user-visible, both already asserted.
- The "skipped exactly once, Pass 3 doesn't rescan" guarantee survives via
  `warns.len() == 1` (L4767), which is a strictly better assertion than the vec
  length (it pins the user-visible emission count).
- No test constructs `LockPlan` or `CloseSetAccumulator` by struct literal with
  this field (`CloseSetAccumulator::default()` everywhere; `LockPlan` only via
  `plan_lock`), so there is no fixture to update beyond the listed assertions.

## Verification

- `just test-rust` -- compiles the `braid-cli` crate (proving no dangling
  reference / move-borrow fallout from the field removal) and runs all `lock.rs`
  planner unit tests, including the migrated assertions. This is the complete
  verification surface; the change is unit-test-only in impact.
- No VM tests required: pure planner-internal refactor, no change to
  systemd/mount/lock-lifecycle behavior. (Per repo guidance, run only the tests
  exercising the touched path for a localized change.)
- Sanity grep after editing: `git grep -n skipped_mappers -- cli/src` should
  return **no matches**. (The archived `plans/impl/` design docs still mention
  the field; leave them untouched -- per AGENTS.md they are point-in-time records,
  not code to track.)
