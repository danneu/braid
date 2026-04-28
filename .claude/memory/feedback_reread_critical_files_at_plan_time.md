---
name: re-read critical files at plan time, not from initial read
description: In plan mode (or any review/planning task), re-read files central to the change at the moment of planning instead of trusting an earlier-conversation read; files mutate between turns and stale reads produce plans against obsolete APIs
type: feedback
originSessionId: 2a9f63af-e9da-4378-86c3-f1c7046af8fd
---
In plan mode (or any review/planning task), re-read the files central to
the change at the moment of planning. Do not trust a Read result from
earlier in the conversation, even when no edit appears to have happened
in this session.

**Why:** During a plan review, I built the entire `monitor.rs` plan
against the API I read at conversation start (`Result<MonitorResult,
MonitorError>`, `Err(e) => exit 2 with no latch`). The reviewer pushed
back with concrete file:line citations showing the file had already been
refactored to return `MonitorResult` directly with a fail-closed
`latch_computation_error` catch-all, and an extensive `#[cfg(test)] mod
tests`. The "fix the silent exit 2" framing was obsolete; what remained
was only a forward-compat exhaustive-match gate. Two plan revisions were
wasted before I re-read the file. Files can change between turns
(external edits, sibling agents, the user themselves) and an "I already
read it" cache is worse than slow.

**How to apply:** The first action in plan-mode Phase 1, and again
whenever a reviewer claims the plan targets obsolete code, is to Read
the central file(s) fresh. If a reviewer says "the API changed" or
"this code is gone" with file:line citations, treat that as a hard
signal to re-read before defending the original analysis. Cheap to
verify, expensive to be wrong about.
