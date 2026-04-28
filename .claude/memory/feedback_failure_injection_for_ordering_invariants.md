---
name: Use failure-injection unit tests for ordering invariants between commit and post-op maintenance
description: For "X must persist before Y runs" invariants, inject a Y-failure and assert X already happened — don't try to observe a running VM phase
type: feedback
originSessionId: 97061139-9857-45bb-a9db-cd35c09d6c1b
---
When the invariant is "the membership commit (or other persistence) must
land before the next maintenance step runs", the robust regression is a
command-layer test that:
1. Lets the commit step succeed.
2. Forces the next maintenance step (soft balance, resize, etc.) to
   fail via a runner that returns a specific exit/error.
3. Asserts that the persistence side-effect (pool.json contents) is
   already current AND the journal (pending-op.json) still exists.

**Why:** I proposed VM tests that observed a "running balance" window
to prove pool.json was written before maintenance. The user pivoted to
forced-failure unit tests because (a) VM tests are brittle to timing
and kernel state, (b) some end-to-end shapes are infeasible (see
`feedback_check_existing_infeasibility_notes.md`), and (c) an
inverted-order regression fails the failure-injection test
deterministically.

**How to apply:** For braid, look for existing peers like
`journal_survives_soft_balance_failure` (`cli/src/remove_missing.rs`)
and `close_runs_before_resize_on_live_replace` (`cli/src/replace.rs`).
Either extend those tests with a `read_membership` assertion on the
post-failure state, or add a sibling test using the same failing-runner
scaffolding. Prefer extending if the same failing-runner already
exercises the seam; add a sibling when the assertion logic differs
materially.
