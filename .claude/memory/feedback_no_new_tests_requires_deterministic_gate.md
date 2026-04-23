---
name: "No new tests" claim requires a deterministic regression gate
description: Before claiming "no new tests needed" for a behavior change, audit each cited test for determinism and what it actually asserts; nondeterministic race tests and helper-level unit tests do not gate the surviving path
type: feedback
originSessionId: 7b9ceb57-f718-4c04-a3f0-04f2b15e2db7
---
When a plan removes one implementation branch and relies on an existing test to cover the surviving path, each cited test must be checked on two axes:

1. **Determinism:** Does the test *always* exercise the path in question, or only sometimes? A concurrent-race test (e.g. two unlocks, assert one wins) may only hit the sequential-loser branch probabilistically -- that does not gate the behavior.

2. **Layer:** Does the test exercise the surviving path end-to-end, or just a helper? A Rust unit test on `plan_open_pool` does not catch a wrapper-vs-CLI wiring regression; a VM test that doesn't separate stdout from stderr does not catch a stream-routing regression.

**Why:** Reviewer flagged a plan that proposed deleting the wrapper's duplicate `mountpoint -q` check (which changes stdout -> stderr for the "already mounted" message) and claimed existing tests covered the surviving path. The cited tests were: a nondeterministic concurrent-unlock test that merged `2>&1`, a helper-level Rust unit test on `plan_open_pool`, and an idempotence VM test that never asserted on message text or exit code. None of them would fail if the remaining path regressed.

**How to apply:** When a plan's "Tests" section says "no new tests needed, existing X covers it":
- For each cited test, state explicitly what it asserts and whether it's deterministic.
- If the change has a user-visible axis (exit code, message text, stream routing, message absence) that no existing test asserts *deterministically*, add one dedicated end-to-end test. Do not rely on incidental coverage.
- Stream-routing changes in particular need `>stdout 2>stderr` split capture; `2>&1` merges hide them.
