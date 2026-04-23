---
name: Don't overclaim what an existing test verifies when citing it as regression coverage
description: In a plan's verification section, only claim an existing test covers what its actual asserts prove; read the test file and map each cited coverage claim to a specific assertion
type: feedback
originSessionId: 59fde6fc-0396-43cd-98d2-4afe82145554
---
When a plan cites an existing test as part of its verification story
("the existing X test will catch regressions in Y"), every claim must be
backed by a specific assertion in that test. Don't say "this existing test
relies on Z continuing to behave correctly" unless Z is actually asserted;
tests that only check pool state / file presence / exit status will
silently pass even if the cited output text disappears.

**Why:** A claim like "replace-live-disk relies on successful close
continuing to print its success line" is false if the test only asserts
`btrfs fi show` contents, mapper absence, data integrity, and membership
-- none of which would fail if the success `eprintln!` disappeared. The
verification section then promises coverage the codebase does not have,
and a real regression ships unnoticed.

**How to apply:** When writing a plan's "Verification" section and citing
an existing test, open the test file and map each claim to a concrete
`assert`/`subtest`. If no assertion backs the claim, either downgrade the
description ("happy-path smoke check only; does not gate output text") or
add the assertion to that test as part of the plan. This is an instance
of the broader rule that cited tests must be deterministic AND exercise
the surviving path at the right layer.
