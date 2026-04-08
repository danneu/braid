---
name: Callsite sweeps must use repo-wide grep, not hand-curated inventories
description: When renaming/refactoring requires updating every reference, sweep tracked files with grep — never trust a curated inventory from a single agent or manual list
type: feedback
---

When a task requires updating every reference to something across the repo (renames, API changes, link updates, etc.), the inventory of "what to update" must come from a repo-wide grep over `git ls-files` with explicit excludes, NOT from a hand-curated list.

**Why:** I planned a doc rename and built the callsite list from a single Explore agent's report. The agent searched `.nix` test files but missed the corresponding `.py` test scripts that referenced the same docs. The plan looked complete but would have left stale references in `tests/cli/remove-inhibits-suspend.py` and `tests/cli/remove-missing-inhibits-suspend.py`. The user (correctly) rejected the plan.

**How to apply:**
- For rename/refactor sweeps: build the file list with `git ls-files | grep -vE '<excludes>'`, then grep that list for the pattern.
- Use word-boundary anchors (`\b...\b`) to avoid partial-word matches.
- Be explicit about what is excluded (`archive/`, `reference/`, scratch files, the plan file itself) and *why* — never silently skip.
- The verification step must re-run the same sweep and assert zero hits.
- Hand-curated inventories are fine for *describing* a small known scope, but never for *defining* what to update.
