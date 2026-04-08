---
name: Split test-tree mutations from docs-only changes
description: When a change deletes/moves test files alongside doc edits, split into two patches and gate the test patch on a mandatory `just test-vm` run — don't hide it under "docs only" verification
type: feedback
---

When planning a change that mixes documentation edits with mutations to the test tree (deleting `.py`/`.nix` test files, moving them to `archive/`, renaming them), do **not** plan it as one omnibus "docs cleanup" change with optional test verification. Split it:

1. **Patch A** — pure prose changes to docs. Verification can be grep + re-read.
2. **Patch B** — test-tree mutation. Verification **must** include a mandatory non-verbose `just test-vm` run against the active check graph.

**Why:** A change that mutates the test tree carries real regression risk even when the deleted files appear "orphaned" — anything the grep missed (lazily-evaluated nix attribute, dynamically-discovered file, plan/fixture indirection) only surfaces under a real eval. Hiding the deletion under a "docs only, no tests needed" header lets it land without exercising the binding verification this repo expects. Splitting also keeps the doc patch trivial to review as prose.

**How to apply:** Whenever a plan's "Critical files" section lists both `docs/**/*.md` edits and `tests/**` deletions, restructure into two patches. Each patch gets its own verification section. The test-tree patch's verification must include `just test-vm` (or equivalent active-check run) as a hard requirement, not "optional sanity sweep."
