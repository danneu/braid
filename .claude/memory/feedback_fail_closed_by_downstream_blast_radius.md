---
name: Fail-closed policy is set by the downstream failure mode, not the preflight shape
description: When a preflight path guards an operation whose own failure is catastrophic (fs read-only, data loss), every input uncertainty must refuse, even if a sibling preflight under the same helper warns-and-proceeds
type: feedback
originSessionId: 6e82ce3f-cc22-4fa9-8ffe-438e39d4c64d
---
When the same preflight helper guards multiple arities and the arities have
asymmetric downstream safety profiles, each branch gets its own error policy.
Do not unify "spawn failed / parser shape error" as a single warn-and-proceed
just because the happy path is shared.

**Why:** In `braid remove`, the `remaining >= 2` path falls through to
`btrfs device remove` which ENOSPCs cleanly when the preflight misses. The
`remaining == 1` path, after the RAID1 -> single balance, can crash the
filesystem to read-only mid-migration, with `pending-op.json` already
committed. A warn-and-proceed on spawn/parse failures in the 2->1 branch
therefore recreates the exact unsafe state the preflight exists to prevent,
while the same policy on the >=2 branch is fine.

Reviewer caught this twice in a row on `plan-a-refactor-that-purrfect-torvalds`:
first on "survivor missing from usage output", then on every
`runner.run`/`parse_*` call in the same 2->1 branch. Both times the fix was
the same: make every uncertainty in that branch a hard
`RemoveError::Validation`.

**How to apply:** When adding a new arm to an existing preflight helper, do
not default to the existing error policy. Ask explicitly: what does the
irreversible step downstream of this new arm do if the preflight was wrong?
If it can corrupt state or leave a committed journal without a validated
guard, every uncertainty in the new arm is fail-closed; do not reuse the
sibling branch's warn-and-proceed.
