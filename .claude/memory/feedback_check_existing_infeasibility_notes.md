---
name: Check existing infeasibility notes before proposing a VM test
description: Before sketching a VM test, grep the same area for existing test/comment notes that say end-to-end is infeasible
type: feedback
originSessionId: 97061139-9857-45bb-a9db-cd35c09d6c1b
---
When planning a new VM-level regression test for an area, search the
existing tests and code for notes about why end-to-end was already
ruled out for that exact shape. In braid, comments like "an end-to-end
VM test of this is infeasible -- ..." appear in the test source for the
function in question (e.g.
`cli/src/replace.rs:3169` for missing-path replace soft balance:
degraded single-profile writes ENOSPC `btrfs replace start`).

**Why:** I proposed a VM test for replace missing-path that the
existing test source had already declared infeasible for a kernel-level
reason. The user had to point this out; the time-cost of `grep -n
"infeasible\|cannot\|not feasible" cli/src/<area>.rs` is essentially
zero compared to the cost of a wrong plan.

**How to apply:** Before writing a VM test plan for any mutating
command, grep `cli/src/<area>.rs` for `infeasible`, and read at least
one existing test in the same `mod tests` for the area to learn what
shapes have been tried and rejected. If a note says VM is infeasible,
the failure-injection unit test at the seam is the right replacement.
