---
name: fold validation into the helper that owns the unsafe primitive
description: When a helper internally performs an unsafe primitive (silent no-op, partial write), fold the precondition check into that helper rather than adding a parallel guard alongside it.
type: feedback
originSessionId: 00ff4d54-57a0-4b0f-843a-72ffed4ef1bb
---
When a helper internally performs an unsafe primitive -- the textbook example
is `HashMap::remove` silently returning `None` on a missing key and the
caller then proceeding to `.insert` -- fold the precondition check into that
helper. Do not add a parallel "validate first" guard that sits next to the
helper at the callsite.

**Why:** On plan-a-fix-for-mossy-bengio.md (cmd_replace orphan pool.json
bug), my first draft added a `validate_old_in_membership` helper alongside an
unchanged `build_replacement_membership`. Reviewer's Medium finding:
"correctness is split across two helpers even though
`build_replacement_membership` is the only place that actually performs the
unsafe silent no-op. That makes future regressions easier." The fix: new
signature for the transform so it takes the info it needs to reject bad
input, and the silent `.remove` becomes `.remove` after a checked lookup.

**How to apply:**
  - Locate the line that does the unsafe thing (e.g. `HashMap::remove`,
    unchecked `expect`, partial write). The helper that owns that line is
    the right place to reject.
  - Widen the helper's signature if needed so it has the data to validate
    (e.g. pass `&ReplaceSource` so the Missing-path devid cross-check can
    live inside the transform).
  - Callsite-specific policy gates still stay at callsites (per
    `feedback_caller_specific_gating_belongs_at_callsites.md`). The
    distinction: caller-specific *policy* (should we do X now?) stays out;
    *primitive-level invariants* (the transform requires X to proceed) fold
    in.
