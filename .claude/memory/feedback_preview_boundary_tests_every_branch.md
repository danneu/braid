---
name: preview boundary tests cover every branch with exact output
description: When a helper is promoted to "the preview boundary" for a CLI contract, every branch of that helper needs an exact-output test -- including the no-op/"nothing to do" branch
type: feedback
originSessionId: c04a4fe4-f8c2-42dd-a3df-3cbefe36be80
---
When a plan extracts a render-string helper (e.g. `render_lock_dry_run`) and
calls it "the preview boundary," the test battery must exercise every
branch of that helper with exact-output (`assert_eq!` or
`starts_with` on a full line) assertions -- not just the interesting
branches.

Specifically: include a test for the empty/`nothing to do.` branch. It is
easy to skip because it "feels trivial," but the render helper can silently
drop, alter, or reroute that branch without any other test failing.
Substring checks and happy-path-only tests leave a user-visible output
unpinned.

**Why:** Dan pushed back twice in a single plan review for this exact
pattern -- first for substring-only assertions, then for a missing no-op
branch test. The principle: "tests pin the actual contract, not just one
signal."

**How to apply:** In plans that introduce a render helper, enumerate its
output branches (warn / steps / no-op / error) and ensure each branch has
at least one test with an exact-string assertion. Weak helper tests
(substring only, only testing one branch) mean the plan is overclaiming
coverage -- flag it as a missing-coverage finding in plan review.
