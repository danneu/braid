---
name: keep repro tests focused — don't bundle unrelated coverage
description: When planning a repro test, resist bundling tangential concerns (e.g. "subsequent replace works") into a test focused on a specific recipe or behavior. The repo already has dedicated coverage for adjacent concerns, and bundling makes failures ambiguous.
type: feedback
---

When planning a repro test, resist bundling tangential concerns into the
test even if the source issue lists them as requirements. A focused test
that fails gives a clear signal about what broke; a bundled test that
fails forces the reader to disambiguate which concern caused it.

**Why:** A draft of plans/wip/pure-frolicking-yao.md (the #46 cleanup-
recipe test) included a "subsequent braid replace works" phase because
issue #46 listed it as one of six requirements. The user pointed out that
the repo already has focused post-recovery replace coverage in
`tests/cli/recover-replace-not-started.py` and
`tests/cli/recover-replace-completed.py`, and bundling another replace
into the cleanup test would dilute its purpose and make a later replace
failure ambiguous about whether the cleanup recipe itself is sound.

**How to apply:** Before adding a phase to a repro test, ask:
1. Does the repo already have dedicated coverage for this concern? If yes,
   don't duplicate — point at the existing test in the plan instead.
2. Does the phase test the same root cause as the rest of the test? If
   not, it belongs in a sibling test, not bundled in.
3. If a phase fails, would the failure clearly point at the test's
   primary concern? If it could be ambiguous, split it out.
4. The lock/unlock/remount cycle is a fine proxy for "the pool is back
   to normal" without dragging in a full replace.

This applies even when the source issue explicitly lists the bundled
concern as a requirement — push back on the issue and split the work
across focused tests.
