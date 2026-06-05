---
intent: How to reason while writing or reviewing a plan -- re-read central files, derive inventories from git ls-files + rg, and verify recipes against current cmd_*/plan_* code and tool behavior.
---

# Planning and review hygiene

- Re-read the central files immediately before writing or reviewing a plan; do
  not rely on earlier conversation reads when code may have changed.
- For renames, refactors, and callsite sweeps, derive the inventory from
  tracked files with `git ls-files` plus `rg`. Be explicit about exclusions and
  rerun the same search as verification.
- Before planning recovery or cleanup recipes, verify every step against the
  current `cmd_*` / `plan_*` code and the relevant tool or kernel behavior.
  Treat issue recipes as hypotheses until the code proves them.
- Architecture docs describe behavioral contracts, not internal helper names.
  Verify wrapper process/lifetime claims from the wrapper code before writing
  docs that depend on them.
- For external-tool exit-code or wording classifiers, trace the specific
  subcommand return path in `reference/`; a shared errno table is not enough to
  prove one invocation's behavior.
