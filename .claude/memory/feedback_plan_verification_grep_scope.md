---
name: Scope verification grep commands in plans to exclude the plan file itself
description: A plan that cites `grep NAME .` as "must return zero matches" is self-failing because the plan file also contains NAME; scope the grep to the code under change
type: feedback
originSessionId: 9800f07a-893b-41ed-8f8f-4b1a667edd9d
---
A plan's verification section often includes a grep like "`rg -n "X" .`
must return zero matches" to confirm that a symbol or test name is
fully removed. That grep will self-match the plan file itself (which
contains the name in its narrative), producing a guaranteed false
failure at implementation time.

**Why:** Caught by Dan in a plan review: the verification step
`grep -rn "old_equals_new_rejects" .` would match the plan's own
narrative. The failure mode is either (a) someone adds an ad-hoc
exclusion at verification time, or (b) the grep is quietly run with
wrong scope and the signal is lost.

**How to apply:**
- When writing a verification grep that asserts "zero matches" for a
  symbol being deleted, scope the grep to the code directory under
  change: `rg -n "NAME" cli/src`, not `rg -rn "NAME" .`.
- Alternatively, explicitly exclude `plans/` and any narrative dirs
  (`docs/`, `TODO*`, `plans/wip/`).
- For a "must appear N times" grep (e.g. "source plus new test"),
  still scope to the code tree -- plan narrative references inflate
  the count.
