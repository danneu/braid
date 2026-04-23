---
name: don't overclaim cross-site parity from a one-sided test
description: "Byte-for-byte parity" between two duplicated call sites requires tests on both sides; a test that only pins one side cannot catch drift on the other
type: feedback
originSessionId: c04a4fe4-f8c2-42dd-a3df-3cbefe36be80
---
When a fix leaves a string duplicated across two call sites (e.g. the
dry-run and real-run branches both printing
`could not scan /dev/mapper for orphans: ... (skipping)`), do not claim
the test "auto-enforces byte-for-byte parity" if the test only pins one
side.

A one-sided test protects only that side. The other side can drift
independently with no failing test. If you want automated parity, factor
the shared string into a single named constant or formatter that both
sides consume. Otherwise, acknowledge the gap honestly in the plan's
verification section and call out that a manual diff/grep is part of the
review.

**Why:** Dan flagged this as a Low finding in plan review -- the plan
claimed "if either side drifts, the test fails," but only the dry-run
side was tested. Overclaiming coverage erodes trust in plan reasoning.

**How to apply:** Before writing "parity is enforced" in a plan, trace
which concrete assertions pin which site. If both sides aren't under
test, either (a) extract a shared constant/formatter and test it, or
(b) be explicit that only one side is pinned and a manual diff check is
required during review.
