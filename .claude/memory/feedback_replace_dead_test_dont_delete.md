---
name: Replace dead tests of real contracts; don't just delete
description: When a test is dead (tautology, no production code exercised) but nominally covers a real user-visible contract, default to replacing it with a real cmd-level regression, not deleting it
type: feedback
originSessionId: 9800f07a-893b-41ed-8f8f-4b1a667edd9d
---
When you discover a dead `#[test]` (e.g. a tautological `assert_eq!(x, x)`
that exercises no production code) whose *name* points at a real
user-visible contract (CLI validation guard, error-path propagation,
user-facing rejection), the default move is to **replace** it with a
real regression test, not delete it. "Delete the dead test" looks like
a simplicity win but turns bad coverage into no coverage for an actual
contract.

**Why:** In a `cmd_replace` review, I proposed deleting
`old_equals_new_rejects` (a `assert_eq!("disk1", "disk1", ...)`
tautology). Dan pushed back: the real `--old == --new` guard was a
user-visible CLI contract with no VM test and no other unit coverage.
Deletion would have left a real-world operator typo protector untested.
The simpler-looking change was silently worse.

**How to apply:**
- Before proposing "delete dead test X", grep for other tests covering
  the same contract X names. If none, the deletion leaves a gap.
- Check whether the contract is user-visible (CLI validation, error
  payload, rejection message). If yes, lean toward replacement.
- Look for existing cmd-level scaffolds in the same test module (mock
  runners, recording inhibitors, tempdir state paths). Cloning a
  neighbor test is usually cheaper than you think -- existing
  scaffolds already handle preflight, UPS, journal, inhibitor seams.
- Pick the scaffold that lets guard-removal produce a *different*
  observable result -- not a scaffold where some other unrelated
  validation fires first and masks the mutation check.
- Include three assertion axes when a guard sits on a seam: typed
  variant+payload (catches deletion), inhibitor/resource state
  (catches seam misplacement), journal/side-effect state (catches
  partial commit). They are not redundant -- different regression
  modes trip different assertions.
- Do a mutation check: temporarily disable the guard, rerun the
  specific test, confirm it fails for the *expected* reason, restore
  the guard. If it still passes, the test doesn't pin the contract.
