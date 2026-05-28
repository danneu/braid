# Plan: fix `DiskState` doc-comment variant count

## Context

A review finding (Low / Project fit) flagged that the `DiskState` enum's
doc comment in `cli/src/doctor.rs:284-287` claims "six reachable
outcomes" while the enum body at `cli/src/doctor.rs:289-313` actually
holds seven variants. The seventh variant, `LuksUuidMismatch`, was
introduced together with the central ADR-024 swap-detection logic in
commit `3ff2ec1 fix(doctor): fail on declared disk luks uuid swaps`.

Git history shows the doc comment was bumped from "four reachable
outcomes" to "six reachable outcomes" in that same commit, but the math
does not work out either way:

- The original style counted *outcome categories* (treating the
  `missing/non-block/probe-failed` slash-group as one bucket), so the
  six-variant enum legitimately read as "four categories".
- After adding `LuksUuidMismatch`, the same outcome-category style would
  give "five"; per-variant counting would give "seven". "Six" matches
  neither model and is internally inconsistent with its own
  parenthetical.

The deeper problem is that any numeric prose count drifts the next time
a variant is added: the enum body is compiler-checked, the number in
the doc is not. An earlier draft of this plan proposed bumping "six" to
"seven", but that fix recreates the same maintenance trap one
generation later. The right move is to drop the count entirely and let
the parenthetical enumerate the variants by name -- the enumeration
itself becomes the audit surface, and the only drift mode is "added a
variant without naming it here", which is far more reviewable than a
hand-maintained integer.

The summarizer (`summarize_declared_disks`, `cli/src/doctor.rs:386`
onward) already renders each variant on its own branch, so an expanded
per-variant parenthetical matches how the type is actually consumed --
no information is lost by dropping the slash-group bundling.

## Recommended change

Rewrite the enum-level doc comment so it (a) drops the numeric count
and (b) expands the slash-grouped parenthetical to name each variant.

**File:** `cli/src/doctor.rs`
**Lines:** 284-287 (doc comment immediately preceding the enum)

**New text:**

```rust
/// Classification of a single declared disk after the doctor's LUKS probe.
/// `summarize_declared_disks` translates a slice of these into a `CheckResult`;
/// the variants pin the rendered declared-disk outcomes (header Ok, UUID mismatch,
/// header unreadable, header damaged, missing, non-block, probe-failed).
```

The change is two edits to one sentence:

1. `the six reachable outcomes` -> `the rendered declared-disk outcomes`
   (drop the count so future variant additions can't desync a number).
2. `missing/non-block/probe-failed` -> `missing, non-block, probe-failed`
   (expand the slash-group so the parenthetical enumerates per
   variant; this is now the audit surface that replaces the count).

No other doc comments, code paths, tests, or design docs reference the
"six" count -- this is a self-contained docs nit.

## Verification

This is a comment-only change with no runtime or behavioral effect.

- `just test-rust` -- confirm the crate still compiles (catches a typo
  inside the `///` block breaking rustdoc only if it produces a warning;
  primarily a smoke check).
- Re-read the enum body and match each name in the new parenthetical
  1:1 against the variants in `cli/src/doctor.rs:289-313`; no variant
  should be missing and no name should fail to match a variant.

No new tests are warranted: the change is in a doc comment that is not
load-bearing for any code path, and the existing
`summarize_declared_disks` unit tests already pin per-variant
rendering.

## Out of scope

- The `///` per-variant docs on each `DiskState` arm. They are accurate
  and do not depend on the enum-level count.
- The `classify_disk_state` and `classify_luks_identity` doc comments;
  they describe behavior, not variant counts.
- ADR-024 / `docs/design/decisions/024-luks-uuid-identity.md` and any
  other design docs -- none of them quote the "six outcomes" phrasing
  (verified by grep against the source tree).
- `plans/impl/2026-05-13-doctor-luks-uuid-swap.md:38` references the
  same off-by-one ("six reachable outcomes (was five)"), but
  `plans/impl/` is an archive of work that has already shipped; do
  not back-edit it.
