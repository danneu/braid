# Ideal docs update for monitor.rs ProbeError gate comment

## Context

`cli/src/monitor.rs:43-72` opens `cmd_monitor` with an exhaustive
match on every `ProbeError` variant returned by `probe_pool_alerts`.
The leading rationale at lines 46-50 is correct and load-bearing: it
declares the match exhaustive on purpose so that any future
`ProbeError` variant produces a compile error and forces the
developer to classify it as offline or fail-closed. monitor is the
headless surface, so a silently-defaulted classification could
propagate into operator-visible behavior.

The inner parenthetical at lines 58-60 reads:

> ... or LUKS-side mismatch (UnsupportedLuksVersion /
> MapperConflict, both unreachable from probe_pool_alerts today
> but listed for the gate).

That parenthetical was accurate when written. Commit `98f0275`
("fix(cli): verify mapper backing paths") later added two more
LUKS-ownership variants -- `MapperBackingMismatch` and
`MapperBackingResolveError` -- to the same match arm
(`cli/src/monitor.rs:68-69`) without touching the comment. The
word "both" now under-counts: four LUKS-side variants are
unreachable from `probe_pool_alerts` today, not two
(`UnsupportedLuksVersion`, raised in `probe_config_disk` at
`cli/src/probe.rs:202-206` as the LUKS2-only invariant check,
plus the three `Mapper*` ownership variants produced by
`From<OwnershipError> for ProbeError` at
`cli/src/probe.rs:116-150`). The "ownership" label only fits the
`Mapper*` subset; `UnsupportedLuksVersion` is a version check, so
the rewrite uses "LUKS-side" as the umbrella term.

This was surfaced by a verify-issue pass on the same arm. The
finding's headline fix (collapse the unreachable variants into a
catch-all) is the wrong shape: it would destroy the very
"force-classify-each-new-variant" gate the leading comment
documents. The real and only fix is to repair the stale parenthetical
so the explanation matches the arm below it.

Intended outcome: a single-line comment edit that is durable against
future LUKS-ownership variants -- i.e. does not need to be touched
again the next time the `Mapper*` family grows.

## Scope

In scope:

- One comment edit inside `cmd_monitor`'s leading block comment.

Out of scope:

- The match arm at `cli/src/monitor.rs:62-71`. Behavior is unchanged
  and the explicit per-variant enumeration is the desired form per
  ADR 014 (`docs/design/decisions/014-alerts.md`, "First-Class
  Alerts for Disk Health") and the leading rationale at lines 46-50.
- The sibling exhaustive arm in `cli/src/lock.rs:762-770`. It has
  the same per-variant enumeration but no drifting prose -- its
  leading rationale at lines 751-753 describes the gate in
  category-agnostic terms ("if a future ProbeError variant lands,
  it must opt in explicitly here"), so there is nothing to fix.
- The leading rationale at `cli/src/monitor.rs:46-50`. It is
  category-agnostic ("Exhaustive over every ProbeError variant on
  purpose ...") and stays accurate.

## The change

File: `cli/src/monitor.rs`, lines 56-61.

Current:

```rust
// All remaining variants describe indeterminate pool state --
// tooling breakage (Cmd/Parse), pool show internally inconsistent
// (PoolDevice), or LUKS-side mismatch (UnsupportedLuksVersion /
// MapperConflict, both unreachable from probe_pool_alerts today
// but listed for the gate). Fail closed per ADR 014: latch
// ComputationError so the wrapper beeps.
```

Proposed:

```rust
// All remaining variants describe indeterminate pool state --
// tooling/probe breakage (Cmd/Parse/MountInfo), pool show
// internally inconsistent (PoolDevice), or LUKS-side validation
// failure (the LUKS-side variants are all unreachable from
// probe_pool_alerts today but listed for the gate). Fail closed
// per ADR 014: latch ComputationError so the wrapper beeps.
```

Rationale for this exact wording:

- Drops the count ("both") so future `Mapper*` additions do not
  re-stale the prose.
- Drops the specific variant names so the comment is not a second,
  hand-maintained mirror of the match arm below it (which is the
  authoritative list, enforced by the compiler).
- Uses "LUKS-side validation failure" as the umbrella label. It
  accurately covers both the LUKS2-only invariant check
  (`UnsupportedLuksVersion`, raised in `probe_config_disk` at
  `cli/src/probe.rs:202-206`) and the three `Mapper*` ownership
  variants (raised via `From<OwnershipError> for ProbeError` at
  `cli/src/probe.rs:116-150`). "LUKS-ownership" would mis-bucket
  `UnsupportedLuksVersion`, which is a version check, not an
  ownership question.
- Promotes `MountInfo` into the "tooling/probe breakage" bucket
  rather than leaving it implicit. The original comment paired
  only Cmd/Parse there and left `ProbeError::MountInfo(_)`
  uncategorized, even though `fstype_at_mount_via_fs` is the very
  first call inside `probe_pool_alerts` and a mountinfo IO failure
  is exactly the kind of tooling/probe breakage the bucket names.
  Now every arm in the match is named in the prose categories.
- Keeps the "listed for the gate" half-sentence because that is the
  load-bearing rationale: the leading block comment at lines 46-50
  declares the gate, and this parenthetical is what tells a reader
  why specific arms exist despite being dead-today.
- Keeps the ADR 014 reference (`docs/design/decisions/014-alerts.md`).

## Critical files

- `cli/src/monitor.rs` -- the only file edited.

Reference reads only (no edits):

- `cli/src/probe.rs:64-114` -- `ProbeError` enum, source of truth
  for the variant set.
- `cli/src/probe.rs:116-150` -- `From<OwnershipError> for
  ProbeError`, source of the `Mapper*` variants.
- `cli/src/probe.rs:296-376` -- `probe_pool_alerts`, the function
  whose reachable variant set the parenthetical describes.
- `cli/src/lock.rs:751-770` -- sibling exhaustive arm, intentionally
  not touched.
- `docs/design/decisions/014-alerts.md` -- ADR referenced from the
  edited comment.

## Verification

This is a pure comment change, so verification is light:

1. `just test-rust` -- confirm the CLI crate still builds and unit
   tests pass. A comment edit must not change behavior; if anything
   fails, the edit slipped outside the comment block.
2. Eye-check that the three prose buckets partition every arm of
   the match directly below -- eight arms, three buckets, nothing
   left over:
   - "tooling/probe breakage (Cmd/Parse/MountInfo)" ->
     `ProbeError::Cmd(_)`, `ProbeError::Parse(_)`, and
     `ProbeError::MountInfo(_)` (lines 63-64, 70).
   - "pool show internally inconsistent (PoolDevice)" ->
     `ProbeError::PoolDevice { .. }` (line 65).
   - "LUKS-side validation failure (the LUKS-side variants)" ->
     `ProbeError::UnsupportedLuksVersion { .. }`,
     `ProbeError::MapperConflict { .. }`,
     `ProbeError::MapperBackingMismatch { .. }`, and
     `ProbeError::MapperBackingResolveError { .. }` (lines 66-69).

No new tests. The drift is in prose; no test currently enumerates
`ProbeError` variants or counts unreachable ones, and adding one
purely to police a comment would be over-engineering. The compiler's
exhaustiveness check on the match arm is the real gate, and it
already catches the case the comment is talking about. The newly
named `MountInfo` bucket is also already pinned behaviorally by
`cmd_monitor_latches_computation_error_on_mountinfo_io_failure`
in monitor's existing test suite, so the prose-to-behavior link is
covered without adding anything.

## Non-goals / explicit nos

- Do not collapse the match arm to a catch-all. The verify-issue
  pass that surfaced this comment drift considered that and rejected
  it: the explicit enumeration is the gate.
- Do not add per-variant comments inside the match. The leading
  block comment plus the rephrased parenthetical are sufficient.
- Do not touch `cli/src/lock.rs`. Its sibling arm is correct and
  its surrounding comments are not stale.
- Do not write a regression test. No behavior changes; no test
  policy is enforceable on prose without inventing one.
