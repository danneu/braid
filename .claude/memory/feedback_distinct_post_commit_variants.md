---
name: Split post-commit error variants by remediation, not by layer
description: When a mutation has multiple post-commit persist steps, each with different on-disk consequences, give each failure its own error variant and message rather than collapsing them
type: feedback
originSessionId: 00109177-91cb-4d3d-b6cd-fa76d9b3ab16
---
Don't collapse distinct post-commit failure modes into one generic variant
when the user's next action differs per mode. In `remove.rs`, a failure of
`membership::save_membership` means `pool.json` is stale but the journal is
authoritative; a failure of `journal::clear_journal` means `pool.json` is
already correct but the *journal* is the thing keeping recovery mode latched.
Those are different stories. A single "state persist failed -- run braid
recover" message is accurate for the first and misleading for the second.

**Why:** Feedback from Dan on the Section-4 slice of
`plans/wip/plan-a-refactor-that-purrfect-torvalds.md`. He rejected a plan that
introduced one shared `PersistFailure(String)` variant covering both
`save_membership` and `clear_journal` failures.

**How to apply:** Before proposing a generic post-commit error variant, list
the post-commit steps and ask: "does the operator's next action differ if
this one fails vs that one?" If yes, give each failure its own variant with a
message that names the specific on-disk state (which file is stale, which
flag is latched). Also: if the change is "error semantics only", add
unit coverage that actually forces each reclassified failure mode and asserts
the variant — compile-only verification does not prove the mapping.
