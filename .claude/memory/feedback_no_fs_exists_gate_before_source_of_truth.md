---
name: Don't gate a source-of-truth query behind fs.exists
description: When a tool (e.g. cryptsetup status) is the real source of truth for a piece of state, query it unconditionally -- don't first check fs.exists and only call the tool when the path is present
type: feedback
originSessionId: c7a096e0-6eca-4482-b8dc-79884364f1a5
---
Don't gate a source-of-truth query behind `fs.exists` (or any cheaper-but-weaker check).

**Why:** In `probe_config_disk`, `mapper_open = fs.exists("/dev/mapper/<name>")` was the original bug. An initial fix proposed wrapping the tool query in `if fs.exists(...) { verify_backing() }`. Dan rejected this: "Wrapping that check in a helper still leaves a TOCTOU split and keeps 'path exists' in the control flow when `cryptsetup status` is the real source of truth." The gate reopens the same TOCTOU window the bug exemplifies, and leaves a weaker check inside the control flow as a hazard for future refactors. `cryptsetup status` on a closed mapper cleanly reports inactive -- a single call covers both closed and open cases.

**How to apply:** When designing a probe/verification function, identify the authoritative source for the state in question. Query that source unconditionally. Do not pre-filter on a cheaper observable (path existence, cached value, etc.) -- the savings are negligible and the control-flow split invites regressions. If the authoritative call is expensive enough to matter, that's a separate discussion about caching, not about gating.
