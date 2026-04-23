---
name: ascii cleanup scope is whole file
description: When a slice removes Unicode from a file to satisfy a repo-wide ASCII rule, fix every occurrence in that file, not just user-facing ones
type: feedback
originSessionId: 7cbc5238-7084-4d24-ae61-87d088ff5674
---
When a slice's purpose is to bring a file into compliance with a repo-wide
writing rule (e.g. the ASCII-everywhere rule in `~/.claude/CLAUDE.md` and
`AGENTS.md`), the scope is "every matching character in that file", not "the
lines the plan happens to enumerate".

**Why:** The repo rule in `~/.claude/CLAUDE.md` says plain ASCII "applies to
everything written: markdown, code comments, chat responses". Shipping a
slice that fixes only user-facing strings while leaving em-dashes in doc
comments, inline comments, and test panic messages leaves the file out of
compliance even after the slice supposedly completes. A reviewer flagged
this as a high-severity finding and required pivoting to a whole-file pass.

**How to apply:** When you see a plan like "replace em-dash/arrow/middle-dot
in cli/src/X.rs at lines A, B, C", treat the line list as a starting
inventory. Before scoping, run the matching grep across the whole file and
include every hit -- production strings, comments, doc comments, test
assertions, panic/expect messages, `{:?}` debug format args. Verification
should be a zero-match grep, not "the specific lines were touched". If the
slice description narrows this, push back and widen rather than shipping
half-compliant.
