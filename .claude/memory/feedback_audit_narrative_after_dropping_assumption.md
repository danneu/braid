---
name: Audit narrative when dropping a test assumption mid-edit
description: When you change a test's premise (e.g. drop a "this scenario produces X" requirement), audit the entire file's descriptive text for stale references that now contradict the new framing — not just the section being directly edited
type: feedback
---

When mid-edit you discover that a test cannot establish a precondition you assumed it could (e.g. "writes-while-degraded create single-profile chunks" turns out to be false in the chosen topology), and you weaken the test to drop that assumption, audit the **entire file** for stale narrative that still implicitly relies on the old assumption. Headers, intent comments, scenario descriptions, "Why it exists" sections, and "What this test verifies" lines all need to come along with the change.

**Why:** A reviewer flagged that after I dropped the "force single-profile chunks" requirement from `tests/cli/remove-missing-inhibits-suspend.py` and added a clarifying observability note, the original Intent/Why text still described the test as protecting a "long-running soft RAID1 balance" and a "partway through suspend" scenario. The new note contradicted the unchanged top-level description, leaving a misleading file. The local fix (adding a note) was correct in isolation but not aligned with the rest of the file's narrative.

**How to apply:** After making the focused edit, re-read the file top to bottom looking for any sentence that depends on the dropped premise. Also re-read any per-test header docblock — the "Intent/Why/Scenario" template is especially prone to this because it's written once at the start and not naturally revisited. Common dependents to check: test name, test description, "What this verifies" bullets, "Why it exists" bullets, scenario narrative, motivation paragraphs. If the new framing weakens the test (e.g. from "protects long-running X" to "verifies wiring for X"), make that downgrade explicit somewhere prominent.
