---
name: Check local/cloned source before web search
description: When researching tool behavior, clone or check local source first rather than web searching
type: feedback
---

Prefer cloning repos or checking local source to answer questions about tool behavior, rather than web searching.

**Why:** User prefers reading actual source over potentially stale web results. The project already has a `reference/` directory of cloned upstream repos for this purpose.

**How to apply:** When investigating how a tool works (e.g., mdBook highlighting), clone the repo or check vendored source first. Only fall back to web search if the source doesn't answer the question.
