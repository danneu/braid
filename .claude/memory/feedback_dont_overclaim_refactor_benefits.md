---
name: Don't overclaim refactor benefits beyond what the type system enforces
description: When pitching a refactor, only claim invariant enforcement that the type design actually provides — `pub` field newtypes don't enforce anything
type: feedback
---

When framing a refactor's rationale, only claim guarantees the type design
actually enforces. A newtype with a public field (e.g.
`pub struct MountPoint(pub String)`) does **not** enforce any invariant —
anyone can fabricate one with arbitrary contents. Don't pitch a signature
cleanup as "fixes a throwable-away invariant" when the invariant was never
enforced to begin with.

**Why:** In the MountPoint calling-convention plan I framed switching helpers
from `&str` to `&MountPoint` as restoring a "this came from config validation"
invariant. The user pointed out that `MountPoint` is publicly constructible,
so the invariant was always decorative — the refactor doesn't change that.
The plan churned signatures while overclaiming the architectural benefit, and
the user asked me to reframe it strictly as an API-consistency + allocation
cleanup.

**How to apply:** Before writing a refactor's "why", ask: what does the type
system actually prevent after this change that it didn't prevent before? If
the answer is "nothing — callers can still construct the type freely", then
the honest framing is API consistency, allocation cleanup, or reduced
boilerplate — not invariant enforcement. Only claim invariant protection
when the refactor adds a checked constructor + private field, or otherwise
narrows the set of constructible values.
