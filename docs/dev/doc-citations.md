---
intent: How to cite files and anchors in ADRs and docs/ prose -- path#symbol for code, path#heading-slug for markdown, never line numbers -- and how to treat references in frozen decision docs.
---

# Doc and ADR file references

In ADRs, decision docs, and `docs/` prose, never reference another file by line
number. Line numbers drift the moment surrounding code or text is edited, so the
pointer silently goes stale and misleads the next reader. Use a `path#anchor`
reference instead -- one shape for both code and docs, where the anchor names
_what_ and the path says _where_:

- **Code** -- `path#symbol` as a plain code span, not a link:
  ``(see `cli/src/cmd/unlock.rs#cmd_unlock`)``. The symbol is a `fn`, `struct`,
  `enum`, `trait`, `impl`, module, or `const`, method-qualified where it helps
  (`cli/src/cmd/plan.rs#Planner::plan`). The symbol is the drift-proof,
  greppable half -- one `rg cmd_unlock` finds both the citation and the
  definition. Never write `cli/src/cmd/unlock.rs:142`, and do not linkify code
  paths: `cli/` lives outside the mdBook root, so a link 404s in the rendered
  book and dodges linkcheck. A bare file path (no `#symbol`) is fine when the
  whole file is the referent.
- **Markdown / mdBook** -- `path#heading-slug` as a real Markdown link, e.g.
  `[...](docs/internals/luks-unlock.md#header-backup-workflow-and-messaging)`,
  not a line number or section count. Unlike code refs these are clickable and
  validated by `mdbook-linkcheck2`, so a renamed heading fails CI instead of
  rotting silently.

A symbol or heading anchor survives edits and is greppable; a line number is
neither. This applies to docs and comments -- transient analysis in `plans/wip/`
is exempt.

This rule governs braid's own tracked files; external upstream code under
`reference/` is gitignored and cited differently (by shape, not `path#symbol`) --
see [Citing reference/ code](reference-source.md#citing-reference-code).

## Decision-doc references

A decision doc with `status: Superseded` or `Deprecated` is a point-in-time
record. Do not rewrite its body or `## See` section to track current code -- the
`> Superseded by ...` banner is the forward pointer to live artifacts. Repointing
a frozen doc's references at today's successor code only makes it contradict its
own narrative.

Independent of status, a `## See` bullet whose path no longer resolves is a broken
pointer, not history. Drop it; or, if the removed file has lasting reference value
(an archived design doc or plan -- not deleted dead code), replace the bare path
with the git-history-note form used in 002 and 003:
`(preserved in git history; last present at commit <hash>)`. The `## See` path
half of this rule is enforced by `scripts/docs/check-see-paths.py`.
